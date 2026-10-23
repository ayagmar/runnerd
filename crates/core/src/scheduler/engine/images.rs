//! The profiles' runner images: before a profile with a pulled `image` starts a runner, the engine checks Docker
//! has the image and pulls it in the background when it does not. A pull that failed is tried again later, longer
//! each time in a row. Images built from `image_stage` are never pulled; a missing one fails the runner's start.
//! At start, after a reload and after a pull, it removes the tags of runnerd's published runner images that no
//! profile runs and no container was made from, one at a time (ADR 0040); it never removes another image.

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::Engine;
use super::runners::backoff;
use crate::config::{Image, PUBLISHED_RUNNER};
use crate::docker::{self, DockerApi, TaggedImage};
use crate::github::GithubApi;

/// Where a pulled image stands.
#[derive(Debug)]
pub(super) enum ImageState {
    Present,
    /// `failures` pulls failed in a row before this one.
    Pulling {
        task: JoinHandle<Result<(), docker::Error>>,
        failures: u32,
    },
    Failed {
        failures: u32,
        retry_at: SystemTime,
    },
}

/// The pulled images of the config's profiles, and the removal of the published runner images no profile runs.
#[derive(Debug)]
pub(super) struct Images {
    /// The pulled images, by reference.
    states: BTreeMap<String, ImageState>,
    /// Whether runner images no profile runs may be left to remove: at start, after a reload or a pull.
    pub(super) stale_due: bool,
    /// The tag being removed, one at a time.
    pub(super) removing: Option<Removal>,
    /// Tags Docker refused to remove since the removals were last due, not tried again until then.
    refused: BTreeSet<String>,
}

/// A tag being removed.
#[derive(Debug)]
pub(super) struct Removal {
    tag: String,
    pub(super) task: JoinHandle<Result<(), docker::Error>>,
}

impl Images {
    /// Nothing pulled yet, and the removal due at start.
    pub(super) fn new() -> Images {
        Images {
            states: BTreeMap::new(),
            stale_due: true,
            removing: None,
            refused: BTreeSet::new(),
        }
    }
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Collects finished pulls and starts the pulls of images the config's profiles need and Docker lacks.
    pub(super) async fn pull_images(&mut self) {
        let finished: Vec<String> = self
            .images
            .states
            .iter()
            .filter(|(_, state)| matches!(state, ImageState::Pulling { task, .. } if task.is_finished()))
            .map(|(image, _)| image.clone())
            .collect();
        for image in finished {
            let Some(ImageState::Pulling { task, failures }) = self.images.states.remove(&image)
            else {
                continue;
            };
            let state = match task.await {
                Ok(Ok(())) => {
                    self.images.stale_due = true;
                    ImageState::Present
                }
                Ok(Err(error)) => failed(&image, failures, &error.to_string()),
                Err(error) => failed(&image, failures, &error.to_string()),
            };
            self.images.states.insert(image, state);
        }
        let wanted: Vec<String> = self
            .config
            .profiles
            .values()
            .filter_map(|profile| match &profile.image {
                Image::Pulled(reference) => Some(reference.clone()),
                Image::Stage(_) => None,
            })
            .collect();
        let now = SystemTime::now();
        for image in wanted {
            let failures = match self.images.states.get(&image) {
                None => 0,
                Some(ImageState::Failed { failures, retry_at }) if *retry_at <= now => *failures,
                Some(_) => continue,
            };
            match self.docker.image_present(&image).await {
                Ok(true) => {
                    self.images.states.insert(image, ImageState::Present);
                }
                Ok(false) => {
                    let docker = self.docker.clone();
                    let reference = image.clone();
                    let task = tokio::spawn(async move { docker.pull(&reference).await });
                    self.images
                        .states
                        .insert(image, ImageState::Pulling { task, failures });
                }
                Err(error) => {
                    let state = failed(&image, failures, &error.to_string());
                    self.images.states.insert(image, state);
                }
            }
        }
        self.remove_stale_images().await;
    }

    /// Removes the next published runner image tag no profile runs, one per tick, in the background. Which one is
    /// decided here, against the config and pulls of now, so a reload or a pull publishing its tag in between counts;
    /// an image a reload wanted while its tag was being removed is checked again, and pulled.
    async fn remove_stale_images(&mut self) {
        if let Some(removal) = &self.images.removing {
            if !removal.task.is_finished() {
                return;
            }
            let Some(Removal { tag, task }) = self.images.removing.take() else {
                return;
            };
            match task.await {
                Ok(Ok(())) => {
                    info!("removed {tag}, which no profile runs");
                    let removed = Reference::parse(&tag);
                    // Present when a reload wanted it while it was being removed: checked again, and pulled.
                    self.images.states.retain(|reference, state| {
                        !matches!(state, ImageState::Present)
                            || Reference::parse(reference).tag_ref() != removed.tag_ref()
                    });
                }
                Ok(Err(error)) => {
                    warn!("cannot remove {tag}: {error}");
                    self.images.refused.insert(tag);
                }
                Err(error) => {
                    warn!("removing {tag} stopped: {error}");
                    self.images.refused.insert(tag);
                }
            }
        }
        if !self.images.stale_due {
            return;
        }
        let Some(tag) = self.next_stale_tag().await else {
            self.images.stale_due = false;
            self.images.refused.clear();
            return;
        };
        let docker = self.docker.clone();
        let reference = tag.clone();
        let task = tokio::spawn(async move { docker.remove_image(&reference).await });
        self.images.removing = Some(Removal { tag, task });
    }

    /// The first tag of runnerd's published runner images that no profile, retired or not, names, no pull is
    /// fetching, no container was made from, and Docker did not refuse to remove since the removals were due.
    /// `None` when there is none, or Docker did not list its images or containers.
    async fn next_stale_tag(&self) -> Option<String> {
        let listed = async {
            let images = self.docker.tagged_images().await?;
            let used = self.docker.container_images().await?;
            Ok::<_, docker::Error>((images, used))
        };
        let (images, used) = match listed.await {
            Ok(listed) => listed,
            Err(error) => {
                warn!("cannot list Docker's images and containers: {error}");
                return None;
            }
        };
        let wanted: Vec<Reference> = self
            .config
            .profiles
            .values()
            .chain(self.retired.values())
            .map(|profile| profile.image.reference())
            .chain(
                self.images
                    .states
                    .iter()
                    .filter(|(_, state)| matches!(state, ImageState::Pulling { .. }))
                    .map(|(reference, _)| reference.clone()),
            )
            .map(|reference| Reference::parse(&reference))
            .collect();
        images
            .iter()
            .filter(|image| !used.contains(&image.id))
            .filter(|image| !wanted.iter().any(|reference| reference.names(image)))
            .flat_map(|image| &image.tags)
            .find(|tag| {
                Reference::parse(tag).name == PUBLISHED_RUNNER
                    && !self.images.refused.contains(*tag)
            })
            .cloned()
    }

    /// Whether a profile's runners must wait for their image: it is pulled and Docker does not have it yet.
    pub(super) fn awaits_image(&self, image: &Image) -> bool {
        match image {
            Image::Stage(_) => false,
            Image::Pulled(reference) => {
                !matches!(self.images.states.get(reference), Some(ImageState::Present))
            }
        }
    }

    /// Forgets that Docker had an image, after a runner failed to start from it: the next tick checks again.
    pub(super) fn recheck_image(&mut self, image: &str) {
        if matches!(self.images.states.get(image), Some(ImageState::Present)) {
            self.images.states.remove(image);
        }
    }
}

/// An image reference as Docker normalises it: Docker Hub's names in full (`docker.io/library/alpine`, from
/// `alpine` or `index.docker.io/alpine`), and the tag `latest` when it has neither tag nor digest.
#[derive(Debug, PartialEq, Eq)]
struct Reference {
    /// The registry and repository, such as `ghcr.io/ayagmar/runnerd-runner`.
    name: String,
    tag: Option<String>,
    digest: Option<String>,
}

impl Reference {
    /// As `distribution/reference`'s `ParseNormalizedNamed` does: the first part is a registry when it has a `.` or
    /// a `:`, is `localhost` or has a capital letter.
    fn parse(reference: &str) -> Reference {
        let (rest, digest) = match reference.split_once('@') {
            Some((rest, digest)) => (rest, Some(digest.to_owned())),
            None => (reference, None),
        };
        let (name, tag) = match rest.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => (name, Some(tag.to_owned())),
            _ => (rest, None),
        };
        let tag = tag.or_else(|| digest.is_none().then(|| "latest".to_owned()));
        let (registry, path) = match name.split_once('/') {
            Some((first, path))
                if first.contains(['.', ':'])
                    || first == "localhost"
                    || first.chars().any(|c| c.is_ascii_uppercase()) =>
            {
                (first, path)
            }
            _ => ("docker.io", name),
        };
        let registry = if registry == "index.docker.io" {
            "docker.io"
        } else {
            registry
        };
        let name = if registry == "docker.io" && !path.contains('/') {
            format!("docker.io/library/{path}")
        } else {
            format!("{registry}/{path}")
        };
        Reference { name, tag, digest }
    }

    /// The name and tag, which name one tag Docker has.
    fn tag_ref(&self) -> (&str, Option<&str>) {
        (&self.name, self.tag.as_deref())
    }

    /// Whether this reference names the image: one of its tags, or with a digest one of its digests.
    fn names(&self, image: &TaggedImage) -> bool {
        let by_tag = self.tag.is_some()
            && image
                .tags
                .iter()
                .any(|tag| Reference::parse(tag).tag_ref() == self.tag_ref());
        let by_digest = self.digest.is_some()
            && image.digests.iter().any(|digest| {
                let digest = Reference::parse(digest);
                digest.name == self.name && digest.digest == self.digest
            });
        by_tag || by_digest
    }
}

/// An image whose pull failed after `failures` failures in a row, waiting to be tried again.
fn failed(image: &str, failures: u32, error: &str) -> ImageState {
    let failures = failures + 1;
    let wait = backoff(failures);
    warn!(
        "cannot pull {image}: {error}; trying again in {}s",
        wait.as_secs()
    );
    ImageState::Failed {
        failures,
        retry_at: SystemTime::now() + wait,
    }
}

#[cfg(test)]
mod tests {
    use super::Reference;

    #[test]
    fn docker_spellings_of_one_image_name_it_alike() {
        for (spelled, listed) in [
            ("index.docker.io/acme/runner:app", "acme/runner:app"),
            ("docker.io/acme/runner", "acme/runner:latest"),
            ("docker.io/library/alpine:3", "alpine:3"),
            ("index.docker.io/alpine", "alpine:latest"),
            ("localhost:5000/runner:app", "localhost:5000/runner:app"),
        ] {
            assert_eq!(
                Reference::parse(spelled),
                Reference::parse(listed),
                "{spelled}"
            );
        }
        for (one, other) in [
            ("ghcr.io/acme/runner:app", "acme/runner:app"),
            ("localhost/runner:app", "runner:app"),
        ] {
            assert_ne!(Reference::parse(one), Reference::parse(other), "{one}");
        }
    }
}
