//! The shared build cache's side of a runner (ADR 0052, 0053): its socket opened before its container starts and closed
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
use crate::config::BuildCacheWriters;
use crate::docker::DockerApi;
use crate::github::{self, GithubApi, JobDetails};

/// A job whose writes are undecided, with what GitHub said of it.
#[derive(Debug)]
pub(super) struct Undecided {
    runner: String,
    repository: String,
    /// Whose runs its repository lets write.
    writers: BuildCacheWriters,
    job: JobDetails,
}

/// What GitHub said of an undecided job: whether it may write, `None` when GitHub could not say, and its
/// repository's default branch, once known.
struct Answer {
    runner: String,
    repository: String,
    writes: Option<Writes>,
    default_branch: Option<String>,
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
                let writers = self
                    .config
                    .repositories
                    .iter()
                    .find(|candidate| candidate.name == *repository)
                    .map(|candidate| candidate.build_cache_writers)
                    .unwrap_or_default();
                Some(Undecided {
                    runner: occupant.runner_name.clone(),
                    repository: repository.clone(),
                    writers,
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
            .serving(read_answers(github, undecided, branches, known))
            .await;
        for answer in answers {
            if let Some(branch) = answer.default_branch {
                self.default_branches
                    .insert(answer.repository.clone(), branch);
            }
            if let (Some(writes), Some(cache)) = (answer.writes, &self.build_cache) {
                cache.decide(&answer.runner, writes);
            }
        }
    }
}

/// Asks GitHub for each undecided job's run, its repository's default branch unless `known` has it, and, for a run
/// that may write, who opened its pull requests and whether the branch has its commit: a tag's run names the tag as
/// its branch.
async fn read_answers<G: GithubApi>(
    github: Arc<G>,
    undecided: Vec<Undecided>,
    branches: Vec<String>,
    known: BTreeMap<String, String>,
) -> Vec<Answer> {
    let (github, branches, known) = (&*github, &branches, &known);
    futures_util::stream::iter(undecided)
        .map(|undecided| async move {
            let default_branch = match known.get(&undecided.repository) {
                Some(branch) => Ok(branch.clone()),
                None => github.default_branch(&undecided.repository).await,
            };
            let writes = decide(github, &undecided, branches, default_branch.as_ref()).await;
            Answer {
                runner: undecided.runner,
                repository: undecided.repository,
                writes,
                default_branch: default_branch.ok(),
            }
        })
        .buffered(CONCURRENT_REQUESTS)
        .collect()
        .await
}

/// Whether an undecided job may write: `Undecided` stays so, and `None` while GitHub does not answer. A job is never
/// allowed on a lookup that failed.
async fn decide<G: GithubApi>(
    github: &G,
    undecided: &Undecided,
    branches: &[String],
    default_branch: Result<&String, &github::Error>,
) -> Option<Writes> {
    let repository = &undecided.repository;
    let trigger = match github.run_trigger(repository, undecided.job.run_id).await {
        Ok(trigger) => trigger,
        Err(error) => {
            warn!(
                "{repository}: cannot read run {}: {error}",
                undecided.job.run_id
            );
            return None;
        }
    };
    let default_branch = match default_branch {
        Ok(branch) => branch,
        Err(error) => {
            warn!("{repository}: cannot read its default branch: {error}");
            return None;
        }
    };
    let mut writing = vec![default_branch.clone()];
    writing.extend(branches.iter().cloned());
    let writes = buildcache::writes(
        &undecided.runner,
        undecided.job.runner_name.as_deref(),
        repository,
        &trigger,
        &writing,
        undecided.writers,
    );
    let (Writes::Allowed, Some(branch)) = (writes, &trigger.head_branch) else {
        return (writes != Writes::Undecided).then_some(writes);
    };
    if trigger.event == buildcache::PULL_REQUEST_EVENT {
        for &number in &trigger.pull_requests {
            match github.pull_request(repository, number).await {
                Ok(pull) if buildcache::opened_by_user(&pull, repository, &trigger) => {}
                Ok(_) => return Some(Writes::Refused),
                Err(error) => {
                    warn!("{repository}: cannot read pull request #{number}: {error}");
                    return None;
                }
            }
        }
    }
    match github
        .on_branch(repository, branch, &trigger.head_sha)
        .await
    {
        Ok(true) => Some(Writes::Allowed),
        Ok(false) => Some(Writes::Refused),
        Err(error) => {
            warn!(
                "{repository}: cannot compare {branch} with run {}: {error}",
                undecided.job.run_id
            );
            None
        }
    }
}
