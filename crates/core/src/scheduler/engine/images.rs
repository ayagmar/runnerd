//! The profiles' runner images: before a profile with a pulled `image` starts a runner, the engine checks Docker
//! has the image and pulls it in the background when it does not. A pull that failed is tried again later, longer
//! each time in a row. Images built from `image_stage` are never pulled; a missing one fails the runner's start.

use std::time::SystemTime;

use tokio::task::JoinHandle;
use tracing::warn;

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

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Collects finished pulls and starts the pulls of images the config's profiles need and Docker lacks.
    pub(super) async fn pull_images(&mut self) {
        let finished: Vec<String> = self
            .images
            .iter()
            .filter(|(_, state)| matches!(state, ImageState::Pulling { task, .. } if task.is_finished()))
            .map(|(image, _)| image.clone())
            .collect();
        for image in finished {
            let Some(ImageState::Pulling { task, failures }) = self.images.remove(&image) else {
                continue;
            };
            let state = match task.await {
                Ok(Ok(())) => ImageState::Present,
                Ok(Err(error)) => failed(&image, failures, &error.to_string()),
                Err(error) => failed(&image, failures, &error.to_string()),
            };
            self.images.insert(image, state);
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
            let failures = match self.images.get(&image) {
                None => 0,
                Some(ImageState::Failed { failures, retry_at }) if *retry_at <= now => *failures,
                Some(_) => continue,
            };
            match self.docker.image_present(&image).await {
                Ok(true) => {
                    self.images.insert(image, ImageState::Present);
                }
                Ok(false) => {
                    let docker = self.docker.clone();
                    let reference = image.clone();
                    let task = tokio::spawn(async move { docker.pull(&reference).await });
                    self.images
                        .insert(image, ImageState::Pulling { task, failures });
                }
                Err(error) => {
                    let state = failed(&image, failures, &error.to_string());
                    self.images.insert(image, state);
                }
            }
        }
    }

    /// Whether a profile's runners must wait for their image: it is pulled and Docker does not have it yet.
    pub(super) fn awaits_image(&self, image: &Image) -> bool {
        match image {
            Image::Stage(_) => false,
            Image::Pulled(reference) => {
                !matches!(self.images.get(reference), Some(ImageState::Present))
            }
        }
    }

    /// Forgets that Docker had an image, after a runner failed to start from it: the next tick checks again.
    pub(super) fn recheck_image(&mut self, image: &str) {
        if matches!(self.images.get(image), Some(ImageState::Present)) {
            self.images.remove(image);
        }
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
