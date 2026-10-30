//! The shared build cache's side of a runner (ADR 0052, 0053): its socket opened before its container starts and closed
//! with it, what it was served stored in the history, and whether its job may write, decided once GitHub places the
//! job on it. It must not start or remove containers, and it never trusts what a runner's log says of its job.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bollard::models::{Mount, MountType};
use futures_util::StreamExt;
use tracing::warn;

use super::tick::CONCURRENT_REQUESTS;
use super::{Engine, Occupant};
use crate::buildcache::{self, ActionsCache, Scopes, Writes};
use crate::cache;
use crate::config::BuildCacheWriters;
use crate::docker::DockerApi;
use crate::github::{self, GithubApi, JobDetails};

/// How long a repository's default branch is taken as known: one renamed while runnerd runs writes within the hour,
/// and the old name stops writing.
const DEFAULT_BRANCH_FOR: Duration = Duration::from_secs(3600);

/// How long a removed runner's uploads wait for GitHub to say which runner ran its job, asked at each poll: GitHub's
/// jobs API can name none for minutes after a job started (ADR 0085). Past it they are dropped.
pub(super) const ENDED_WAIT: Duration = Duration::from_secs(600);

/// A repository's default branch, and when GitHub said it.
#[derive(Debug, Clone)]
pub(super) struct DefaultBranch {
    branch: String,
    pub(super) read_at: SystemTime,
}

/// A removed runner's uploads held while its job's writes are undecided.
#[derive(Debug)]
pub(super) struct EndedUploads {
    pub(super) repository: String,
    pub(super) job: u64,
    /// When its runner was removed: its uploads wait `ENDED_WAIT` from then.
    pub(super) ended_at: SystemTime,
}

/// A job whose writes are undecided, with what GitHub said of it.
#[derive(Debug)]
pub(super) struct Undecided {
    runner: String,
    repository: String,
    /// Whose runs its repository lets write.
    writers: BuildCacheWriters,
    job: JobDetails,
}

/// What GitHub said of an undecided job: whether it may write, `None` when GitHub could not say, the scopes of a job
/// that may, and its repository's default branch, once known.
struct Answer {
    runner: String,
    repository: String,
    writes: Option<Writes>,
    scopes: Scoped,
    default_branch: Option<String>,
}

/// Whether a job may write and, when it may, the refs of its own and its pull request's base scopes, as GitHub said.
struct Decision {
    writes: Writes,
    scopes: Scoped,
}

/// A writing job's own scope, and its pull request's base, as refs.
#[derive(Default)]
struct Scoped {
    own: Option<String>,
    base: Option<String>,
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// What a runner container in `slot` (1-based) of `profile` mounts: the caches, and its build cache socket's
    /// directory when the build cache is on for its repository's pool in `profile`.
    pub(super) fn runner_mounts(
        &self,
        profile: &str,
        slot: usize,
        runner: &str,
        repository: &str,
    ) -> Vec<Mount> {
        let mut mounts = cache::runner_mounts(&self.names, profile, slot);
        mounts.extend(self.attach_build_cache(runner, repository, profile));
        mounts
    }

    /// Closes the build cache of a runner that did not start.
    pub(super) fn close_build_cache(&self, runner: &str) {
        if let Some(cache) = &self.build_cache {
            cache.detach(runner);
        }
    }

    /// Opens the build cache for a runner about to start, when on for its repository's pool in `profile`, and
    /// returns what its container mounts. Without it, the runner builds without the shared cache.
    pub(super) fn attach_build_cache(
        &self,
        runner: &str,
        repository: &str,
        profile: &str,
    ) -> Option<Mount> {
        let cache = self.build_cache.as_ref()?;
        let wanted = self.config.caches.build.enabled
            && self
                .config
                .pool_in(repository, profile)
                .is_some_and(|pool| pool.build_cache);
        if !wanted {
            return None;
        }
        let actions = if self.config.caches.build.actions_cache {
            ActionsCache::Served
        } else {
            ActionsCache::Off
        };
        match cache.attach(runner, repository, actions) {
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

    /// Closes a removed runner's build cache and records what it was served and stored. One that holds uploads
    /// while its `job`'s writes are undecided, as a job that ended before a poll asked GitHub of it, keeps them
    /// without its socket until `follow_jobs` has had GitHub's decision, for at most `ENDED_WAIT`: they are kept or
    /// dropped by GitHub's answer, not dropped because the job was short or GitHub slow to name its runner.
    pub(super) fn detach_build_cache(&mut self, occupant: &Occupant, job: Option<u64>) {
        let Some(cache) = &self.build_cache else {
            return;
        };
        if let Some(job) = job
            && cache.close(&occupant.runner_name)
        {
            self.ended_uploads.insert(
                occupant.runner_name.clone(),
                EndedUploads {
                    repository: occupant.repository.clone(),
                    job,
                    ended_at: SystemTime::now(),
                },
            );
            return;
        }
        if let Some(counts) = cache.detach(&occupant.runner_name) {
            self.store.build_cache(&occupant.runner_name, counts);
        }
    }

    /// The removed runners `follow_jobs` asked GitHub about whose writes it decided, or that waited `ENDED_WAIT`:
    /// their build cache closes. The others wait for the next poll, holding no slot.
    pub(super) fn detach_ended(&mut self) {
        let Some(cache) = &self.build_cache else {
            self.ended_uploads.clear();
            return;
        };
        let done: Vec<String> = self
            .ended_uploads
            .iter()
            .filter(|(runner, ended)| {
                !cache.undecided(runner)
                    || !ended
                        .ended_at
                        .elapsed()
                        .is_ok_and(|waited| waited < ENDED_WAIT)
            })
            .map(|(runner, _)| runner.clone())
            .collect();
        for runner in done {
            self.ended_uploads.remove(&runner);
            if let Some(counts) = self
                .build_cache
                .as_ref()
                .and_then(|cache| cache.detach(&runner))
            {
                self.store.build_cache(&runner, counts);
            }
        }
    }

    /// Of the jobs GitHub just described, running or ended with uploads held, those whose writes are undecided.
    pub(super) fn undecided(&self, described: &[(String, u64, JobDetails)]) -> Vec<Undecided> {
        let Some(cache) = &self.build_cache else {
            return Vec::new();
        };
        let ended = self
            .ended_uploads
            .iter()
            .map(|(runner, ended)| (runner.as_str(), Some(ended.job)));
        self.occupied()
            .map(|(_, occupant)| (occupant.runner_name.as_str(), occupant.job))
            .chain(ended)
            .filter(|(runner, _)| cache.undecided(runner))
            .filter_map(|(runner, job)| {
                let (repository, _, details) =
                    described.iter().find(|(_, id, _)| job == Some(*id))?;
                // The same in each of its pools.
                let writers = self
                    .config
                    .repositories
                    .iter()
                    .find(|candidate| candidate.name == *repository)
                    .map(|candidate| candidate.build_cache_writers)
                    .unwrap_or_default();
                Some(Undecided {
                    runner: runner.to_owned(),
                    repository: repository.clone(),
                    writers,
                    job: details.clone(),
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
        let known = self.known_default_branches();
        let github = self.github.clone();
        let answers = self
            .serving(read_answers(github, undecided, branches, known))
            .await;
        self.take_answers(answers);
    }

    /// The default branches read in the last `DEFAULT_BRANCH_FOR`, by repository; the others are asked again.
    fn known_default_branches(&self) -> BTreeMap<String, String> {
        self.default_branches
            .iter()
            .filter(|(_, known)| {
                known
                    .read_at
                    .elapsed()
                    .is_ok_and(|age| age < DEFAULT_BRANCH_FOR)
            })
            .map(|(repository, known)| (repository.clone(), known.branch.clone()))
            .collect()
    }

    /// Records what GitHub answered of undecided jobs: their repositories' default branches, and each job's writes.
    fn take_answers(&mut self, answers: Vec<Answer>) {
        for answer in answers {
            if let Some(branch) = &answer.default_branch {
                self.default_branches.insert(
                    answer.repository.clone(),
                    DefaultBranch {
                        branch: branch.clone(),
                        read_at: SystemTime::now(),
                    },
                );
            }
            if let (Some(writes), Some(cache)) = (answer.writes, &self.build_cache) {
                let scopes = Scopes {
                    own: answer.scopes.own,
                    base: answer.scopes.base,
                    default: answer.default_branch.map(|branch| branch_ref(&branch)),
                };
                cache.decide(&answer.runner, writes, scopes);
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
                None => github
                    .repository(&undecided.repository)
                    .await
                    .map(|details| details.default_branch),
            };
            let decision = decide(github, &undecided, branches, default_branch.as_ref()).await;
            let (writes, scopes) = match decision {
                Some(decision) => (Some(decision.writes), decision.scopes),
                None => (None, Scoped::default()),
            };
            Answer {
                runner: undecided.runner,
                repository: undecided.repository,
                writes,
                scopes,
                default_branch: default_branch.ok(),
            }
        })
        .buffered(CONCURRENT_REQUESTS)
        .collect()
        .await
}

/// Whether an undecided job may write, with its scopes: `Undecided` stays so, and `None` while GitHub does not
/// answer. A job is never allowed on a lookup that failed. A pull request's run is scoped by its pull request,
/// reading its base branch next, never by its head branch, which may share a name with any branch.
async fn decide<G: GithubApi>(
    github: &G,
    undecided: &Undecided,
    branches: &[String],
    default_branch: Result<&String, &github::Error>,
) -> Option<Decision> {
    let decision = |writes| Decision {
        writes,
        scopes: Scoped::default(),
    };
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
        return (writes != Writes::Undecided).then(|| decision(writes));
    };
    let mut scopes = Scoped {
        own: Some(branch_ref(branch)),
        base: None,
    };
    if trigger.event == buildcache::PULL_REQUEST_EVENT {
        scopes.own = None;
        for &number in &trigger.pull_requests {
            match github.pull_request(repository, number).await {
                // Its scope is its pull request's, when GitHub ties it to one: of several, none is its own.
                Ok(pull) if buildcache::opened_by_user(&pull, repository, &trigger) => {
                    if let [_] = trigger.pull_requests.as_slice() {
                        scopes = Scoped {
                            own: Some(format!("refs/pull/{number}/merge")),
                            base: Some(branch_ref(&pull.base_branch)),
                        };
                    }
                }
                Ok(_) => return Some(decision(Writes::Refused)),
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
        Ok(true) => Some(Decision {
            writes: Writes::Allowed,
            scopes,
        }),
        Ok(false) => Some(decision(Writes::Refused)),
        Err(error) => {
            warn!(
                "{repository}: cannot compare {branch} with run {}: {error}",
                undecided.job.run_id
            );
            None
        }
    }
}

/// The ref of `branch`, which names a scope.
fn branch_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}
