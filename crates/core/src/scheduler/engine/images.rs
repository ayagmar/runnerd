//! The profiles' runner images: before a profile with a pulled `image` starts a runner, the engine checks Docker
//! has the image and pulls it in the background when it does not. A pull that failed is tried again later, longer
//! each time in a row. Images built from `image_stage` are never pulled; a missing one fails the runner's start.
//! At start, after a reload and after a pull, it removes the tags of runner images no profile runs (ADR 0040).

use std::collections::BTreeMap;
use std::time::SystemTime;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::Engine;
use super::runners::backoff;
use crate::config::Image;
use crate::docker::{self, DockerApi};
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

/// The pulled images of the config's profiles, and the removal of the runner images no profile runs.
#[derive(Debug)]
pub(super) struct Images {
    /// The pulled images, by reference.
    states: BTreeMap<String, ImageState>,
    /// Whether the runner images no profile runs are to be removed: at start, after a reload or a pull.
    pub(super) stale_due: bool,
    /// Their running removal.
    pub(super) removing: Option<JoinHandle<()>>,
}

impl Images {
    /// Nothing pulled yet, and the removal due at start.
    pub(super) fn new() -> Images {
        Images {
            states: BTreeMap::new(),
            stale_due: true,
            removing: None,
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
        self.remove_stale_images();
    }

    /// Removes in the background the runner images no profile runs, when due and not already removing them.
    fn remove_stale_images(&mut self) {
        if !self.images.stale_due
            || self
                .images
                .removing
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        self.images.stale_due = false;
        // A retired profile may still reseed from its image.
        let wanted: Vec<String> = self
            .config
            .profiles
            .values()
            .chain(self.retired.values())
            .map(|profile| profile.image.reference())
            .collect();
        let docker = self.docker.clone();
        self.images.removing = Some(tokio::spawn(async move {
            let tags = match docker.image_tags().await {
                Ok(tags) => tags,
                Err(error) => {
                    warn!("cannot list Docker's images: {error}");
                    return;
                }
            };
            for tag in stale_tags(&tags, &wanted) {
                // Docker refuses to remove the image of a container, running or not; a record's image keeps the
                // layers it was made from.
                match docker.remove_image(&tag).await {
                    Ok(()) => info!("removed {tag}, which no profile runs"),
                    Err(error) => warn!("cannot remove {tag}: {error}"),
                }
            }
        }));
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

/// The last part of a runner image's repository, wherever it is published: `runnerd-runner` for the stages built
/// on this host, `ghcr.io/ayagmar/runnerd-runner` for the published ones.
const RUNNER_REPOSITORY: &str = "runnerd-runner";

/// The tags among `tags` of a runner image repository that no reference in `wanted` names.
fn stale_tags(tags: &[String], wanted: &[String]) -> Vec<String> {
    tags.iter()
        .filter(|tag| repository(tag).rsplit('/').next() == Some(RUNNER_REPOSITORY))
        .filter(|tag| !wanted.iter().any(|reference| names(reference, tag)))
        .cloned()
        .collect()
}

/// Whether an image reference, as a profile has it, names the tag as Docker lists it. A reference with a digest
/// keeps every tag of its repository: Docker may list the image under any of them.
fn names(reference: &str, tag: &str) -> bool {
    let (name, digest) = match reference.split_once('@') {
        Some((name, _)) => (name, true),
        None => (reference, false),
    };
    // Docker lists Docker Hub's images by their short names.
    let name = name.strip_prefix("docker.io/").unwrap_or(name);
    let name = name.strip_prefix("library/").unwrap_or(name);
    if digest {
        repository(name) == repository(tag)
    } else if repository(name) == name {
        tag.strip_suffix(":latest") == Some(name)
    } else {
        name == tag
    }
}

/// A reference without its tag: `ghcr.io/ayagmar/runnerd-runner` for `ghcr.io/ayagmar/runnerd-runner:node-0.0.7`.
fn repository(reference: &str) -> &str {
    match reference.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => repository,
        _ => reference,
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
