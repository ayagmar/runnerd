//! The shared build cache's side of a runner (ADR 0052): its socket opened before its container starts and closed
//! with it, what it was served stored in the history, and whether its job may write, decided once GitHub places the
//! job on it. It must not start or remove containers, and it never trusts what a runner's log says of its job.

use std::collections::BTreeMap;
use std::sync::Arc;

use bollard::models::{Mount, MountType};
use futures_util::StreamExt;
use tracing::warn;

use super::tick::CONCURRENT_REQUESTS;
use super::{Engine, Occupant};
use crate::buildcache::{self, Writes};
use crate::cache;
use crate::docker::DockerApi;
use crate::github::{self, GithubApi, JobDetails, RunTrigger};

/// A job whose writes are undecided, with what GitHub said of it.
#[derive(Debug)]
pub(super) struct Undecided {
    runner: String,
    repository: String,
    job: JobDetails,
}

/// What GitHub says of an undecided job's run, and of its repository's default branch when the config names no
/// branches.
struct Answer {
    undecided: Undecided,
    trigger: Result<RunTrigger, github::Error>,
    default_branch: Option<Result<String, github::Error>>,
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// What a runner container in `slot` (1-based) of `profile` mounts: the caches, and its build cache socket's
    /// directory when the build cache is on for its repository.
    pub(super) fn runner_mounts(
        &self,
        profile: &str,
        slot: usize,
        runner: &str,
        repository: &str,
    ) -> Vec<Mount> {
        let mut mounts = cache::runner_mounts(&self.names, profile, slot);
        mounts.extend(self.attach_build_cache(runner, repository));
        mounts
    }

    /// Closes the build cache of a runner that did not start.
    pub(super) fn close_build_cache(&self, runner: &str) {
        if let Some(cache) = &self.build_cache {
            cache.detach(runner);
        }
    }

    /// Opens the build cache for a runner about to start, when on for its repository, and returns what its
    /// container mounts. Without it, the runner builds without the shared cache.
    pub(super) fn attach_build_cache(&self, runner: &str, repository: &str) -> Option<Mount> {
        let cache = self.build_cache.as_ref()?;
        let wanted = self.config.caches.build.enabled
            && self
                .config
                .repositories
                .iter()
                .any(|candidate| candidate.name == repository && candidate.build_cache);
        if !wanted {
            return None;
        }
        match cache.attach(runner, repository) {
            Ok(dir) => Some(Mount {
                target: Some(buildcache::MOUNT.to_owned()),
                source: Some(dir.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(true),
                ..Default::default()
            }),
            Err(error) => {
                warn!("{runner}: cannot open the build cache: {error}");
                None
            }
        }
    }

    /// Closes a removed runner's build cache and records what it was served and stored.
    pub(super) fn detach_build_cache(&self, occupant: &Occupant) {
        let Some(cache) = &self.build_cache else {
            return;
        };
        if let Some(counts) = cache.detach(&occupant.runner_name) {
            self.store.build_cache(&occupant.runner_name, counts);
        }
    }

    /// Of the running jobs GitHub just described, those whose runner's writes are undecided.
    pub(super) fn undecided(&self, described: &[(String, u64, JobDetails)]) -> Vec<Undecided> {
        let Some(cache) = &self.build_cache else {
            return Vec::new();
        };
        self.occupied()
            .filter(|(_, occupant)| cache.undecided(&occupant.runner_name))
            .filter_map(|(_, occupant)| {
                let (repository, _, job) = described
                    .iter()
                    .find(|(_, id, _)| occupant.job == Some(*id))?;
                Some(Undecided {
                    runner: occupant.runner_name.clone(),
                    repository: repository.clone(),
                    job: job.clone(),
                })
            })
            .collect()
    }

    /// Decides whether each undecided job may write, from its run's trigger and the branches that may.
    pub(super) async fn decide_writes(&mut self, undecided: Vec<Undecided>) {
        if undecided.is_empty() {
            return;
        }
        let branches = self.config.caches.build.branches.clone();
        let known = self.default_branches.clone();
        let github = self.github.clone();
        let answers = self
            .serving(read_triggers(github, undecided, branches.is_empty(), known))
            .await;
        for answer in answers {
            let Answer {
                undecided,
                trigger,
                default_branch,
            } = answer;
            let trigger = match trigger {
                Ok(trigger) => trigger,
                Err(error) => {
                    warn!(
                        "{}: cannot read run {}: {error}",
                        undecided.repository, undecided.job.run_id
                    );
                    continue;
                }
            };
            let branches = match default_branch {
                None => branches.clone(),
                Some(Ok(branch)) => {
                    self.default_branches
                        .insert(undecided.repository.clone(), branch.clone());
                    vec![branch]
                }
                Some(Err(error)) => {
                    warn!(
                        "{}: cannot read its default branch: {error}",
                        undecided.repository
                    );
                    continue;
                }
            };
            let writes = buildcache::writes(
                &undecided.runner,
                undecided.job.runner_name.as_deref(),
                &undecided.repository,
                &trigger,
                &branches,
            );
            if writes == Writes::Undecided {
                continue;
            }
            if let Some(cache) = &self.build_cache {
                cache.decide(&undecided.runner, writes);
            }
        }
    }
}

/// Asks GitHub for each undecided job's run, and for its repository's default branch unless `known` has it or
/// none is needed.
async fn read_triggers<G: GithubApi>(
    github: Arc<G>,
    undecided: Vec<Undecided>,
    needs_default_branch: bool,
    known: BTreeMap<String, String>,
) -> Vec<Answer> {
    let github = &*github;
    let known = &known;
    futures_util::stream::iter(undecided)
        .map(|undecided| async move {
            let trigger = github
                .run_trigger(&undecided.repository, undecided.job.run_id)
                .await;
            let default_branch = if needs_default_branch {
                Some(match known.get(&undecided.repository) {
                    Some(branch) => Ok(branch.clone()),
                    None => github.default_branch(&undecided.repository).await,
                })
            } else {
                None
            };
            Answer {
                undecided,
                trigger,
                default_branch,
            }
        })
        .buffered(CONCURRENT_REQUESTS)
        .collect()
        .await
}
