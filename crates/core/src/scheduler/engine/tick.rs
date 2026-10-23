//! One tick: collects finished work, polls GitHub for runners then queued jobs, plans with `scheduler::plan` and
//! applies the decisions. It owns the memory budget's arithmetic and deletes this machine's runners no slot
//! holds; starting and removing a slot's runner is `runners`. It lists every repository at once, a few requests at
//! a time, and answers the API's commands while it waits for the listings (ADR 0026).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::StreamExt;
use tracing::{info, warn};

use super::{Engine, RunningJobs, Shutdown};
use crate::docker::DockerApi;
use crate::github::{self, Deleted, GithubApi, QueuedJob, Runner};
use crate::scheduler::{Decision, Profile, Repository, RunnerSlot, Slot, plan};

/// GitHub requests a tick has in flight at most. GitHub asks clients not to flood it with concurrent requests;
/// a few keep a tick over many repositories short.
pub(super) const CONCURRENT_REQUESTS: usize = 4;

/// One answer of GitHub's per repository.
type Answers<T> = Vec<(String, Result<T, github::Error>)>;

/// What a tick's GitHub listings returned, by repository.
struct Listings {
    runners: Answers<Vec<Runner>>,
    queued: Answers<Vec<QueuedJob>>,
}

/// Lists the runners of `runners`, then the queued jobs of `queued`, `CONCURRENT_REQUESTS` at a time. Owns what it
/// needs, so the engine answers commands while it runs.
async fn list<G: GithubApi>(github: Arc<G>, runners: Vec<String>, queued: Vec<String>) -> Listings {
    let github = &*github;
    let runners = futures_util::stream::iter(runners)
        .map(|repository| async move {
            let answer = github.runners(&repository).await;
            (repository, answer)
        })
        .buffered(CONCURRENT_REQUESTS)
        .collect()
        .await;
    // Only once every runner listing is in: see `tick`.
    let queued = futures_util::stream::iter(queued)
        .map(|repository| async move {
            let answer = github.queued_jobs(&repository).await;
            (repository, answer)
        })
        .buffered(CONCURRENT_REQUESTS)
        .collect()
        .await;
    Listings { runners, queued }
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    pub(super) async fn tick(&mut self, mode: Shutdown) {
        self.collect_reseeds().await;
        self.collect_trim().await;
        self.collect_finished().await;
        self.kill_cancelled().await;
        // Runners first: a runner that takes its job after this poll still counts as idle against a job the later
        // queued-jobs poll no longer lists, which leaves it surplus at worst (it stops after IDLE_TIMEOUT). The other
        // order sees the job queued and its runner busy, and starts a second runner for it.
        let runners = self
            .repositories
            .keys()
            .chain(&self.draining)
            .cloned()
            .collect();
        let queued = match mode {
            Shutdown::No => self
                .config
                .repositories
                .iter()
                .map(|repository| repository.name.clone())
                .collect(),
            Shutdown::Graceful | Shutdown::Force => Vec::new(),
        };
        let listings = self
            .serving(list(self.github.clone(), runners, queued))
            .await;
        self.apply_runners(listings.runners).await;
        if mode == Shutdown::No {
            self.apply_queued(listings.queued);
        } else {
            for state in self.repositories.values_mut() {
                state.listed = false;
            }
        }
        self.take_found();
        self.follow_jobs().await;
        self.count_mirror_bytes().await;
        if mode == Shutdown::No {
            self.pull_images().await;
        }
        let decisions = match mode {
            Shutdown::No => self.plan(),
            Shutdown::Graceful | Shutdown::Force => self
                .occupied()
                .filter(|(_, occupant)| mode == Shutdown::Force || !occupant.busy)
                .map(|(slot, _)| Decision::Stop { slot })
                .collect(),
        };
        for decision in decisions {
            match decision {
                Decision::Stop { slot } => {
                    let jobs = match mode {
                        Shutdown::Force => RunningJobs::Kill,
                        Shutdown::No | Shutdown::Graceful => RunningJobs::Keep,
                    };
                    self.stop(&slot, jobs).await;
                }
                Decision::Start { slot, repository } => self.start(&slot, &repository).await,
            }
        }
        self.idle_caches(mode).await;
        self.prune();
    }

    fn plan(&self) -> Vec<Decision> {
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        let repositories: Vec<Repository> = self
            .config
            .repositories
            .iter()
            .map(|repository| {
                let state = &self.repositories[&repository.name];
                Repository {
                    name: repository.name.clone(),
                    profile: repository.profile.clone(),
                    labels: repository.labels.clone(),
                    paused: self.control.is_paused(&repository.name),
                    held_until: state.held_until,
                    // GitHub lists a job queued for a few seconds after a runner took it: without this, a tick in
                    // between starts a second runner for it.
                    queued: state
                        .queued
                        .iter()
                        .filter(|job| !taken.contains(&job.id))
                        .cloned()
                        .collect(),
                }
            })
            .collect();
        let profiles = self
            .config
            .profiles
            .iter()
            .map(|(name, profile)| {
                let slots = &self.slots[name];
                // A reload may have left fewer slots than runners: free slots count only up to the configured number.
                let mut free = (profile.slots as usize)
                    .saturating_sub(slots.iter().filter(|slot| slot.is_some()).count());
                let slots = slots
                    .iter()
                    .map(|slot| match slot {
                        None if free > 0 => {
                            free -= 1;
                            Slot::Free
                        }
                        None => Slot::Closed,
                        Some(occupant) => Slot::Runner(RunnerSlot {
                            repository: occupant.repository.clone(),
                            busy: occupant.busy,
                            started_at: occupant.started_at,
                        }),
                    })
                    .collect();
                let profile = Profile {
                    memory: profile.memory.0,
                    preparing: self.is_reseeding(name) || self.awaits_image(&profile.image),
                    slots,
                };
                (name.clone(), profile)
            })
            .collect();
        let free_memory = self
            .memory_max
            .map(|max| max.saturating_sub(self.memory_in_use()));
        let mut decisions = plan(&repositories, &profiles, free_memory, SystemTime::now());
        // Waiting runners a reload left outside the config: their repository is gone or uses another profile now.
        decisions.extend(
            self.occupied()
                .filter(|(slot, occupant)| {
                    !occupant.busy
                        && occupant.job.is_none()
                        && !self.config.repositories.iter().any(|repository| {
                            repository.name == occupant.repository
                                && repository.profile == slot.profile
                        })
                })
                .map(|(slot, _)| Decision::Stop { slot }),
        );
        decisions
    }

    /// Memory caps of the runners, the seed containers (which run with their profile's limits) and the mirror.
    pub(super) fn memory_in_use(&self) -> u64 {
        let runners: u64 = self.occupied().map(|(_, occupant)| occupant.memory).sum();
        let reseeds: u64 = self.reseed_memory.values().sum();
        runners + reseeds + self.mirror_memory_in_use()
    }

    /// The mirror's part of `memory_in_use`.
    pub(super) fn mirror_memory_in_use(&self) -> u64 {
        // The configured mirror is reserved for; a running one keeps the cap it started with until it stops.
        let mirror = &self.config.caches.mirror;
        let configured = if mirror.enabled { mirror.memory.0 } else { 0 };
        let running = if self.mirror_up {
            self.mirror_memory
        } else {
            0
        };
        configured.max(running)
    }

    /// Takes each repository's queued jobs from its listing; a repository a command removed meanwhile is skipped.
    fn apply_queued(&mut self, listings: Answers<Vec<QueuedJob>>) {
        for (repository, answer) in listings {
            let mut listed = false;
            let queued = match answer {
                Ok(queued) => {
                    self.rate_limit_logged = false;
                    listed = true;
                    queued
                }
                Err(github::Error::RateLimited { until }) => {
                    if !self.rate_limit_logged {
                        warn!(
                            "GitHub's rate limit is used up; polling again in {}s",
                            until
                                .duration_since(SystemTime::now())
                                .unwrap_or_default()
                                .as_secs()
                        );
                        self.rate_limit_logged = true;
                    }
                    Vec::new()
                }
                Err(error) => {
                    warn!("{repository}: cannot list queued jobs: {error}");
                    Vec::new()
                }
            };
            if let Some(state) = self.repositories.get_mut(&repository) {
                state.queued = queued;
                state.listed = listed;
            }
        }
    }

    /// Updates each runner's busy flag from the listings and deletes this host's runners that no slot holds.
    async fn apply_runners(&mut self, listings: Answers<Vec<Runner>>) {
        for (repository, answer) in listings {
            // Whether this machine still has a runner registered on it.
            let mut left = false;
            let runners = match answer {
                Ok(runners) => runners,
                Err(github::Error::RateLimited { .. }) => continue,
                Err(error) => {
                    warn!("{repository}: cannot list runners: {error}");
                    continue;
                }
            };
            for runner in runners {
                let held = self
                    .slots
                    .values_mut()
                    .flatten()
                    .flatten()
                    .find(|occupant| occupant.runner_id == runner.id);
                match held {
                    Some(occupant) => {
                        occupant.busy = runner.busy;
                        left = true;
                    }
                    // Only this machine's names: other machines serve the same repositories.
                    None if !runner.name.starts_with(&self.runner_prefix) => {}
                    None => match self.github.delete_runner(&repository, runner.id).await {
                        Ok(Deleted::Deleted) => {
                            info!(
                                "{repository}: deleted {}, which had no container",
                                runner.name
                            );
                        }
                        Ok(Deleted::NotFound) => {}
                        Ok(Deleted::Busy) => {
                            left = true;
                            if self.busy_orphans.insert(runner.id) {
                                warn!(
                                    "{repository}: {} has no container, but GitHub says it runs a job; \
                                     deleting it once GitHub lets go",
                                    runner.name
                                );
                            }
                        }
                        Err(error) => {
                            left = true;
                            warn!("{repository}: cannot delete {}: {error}", runner.name);
                        }
                    },
                }
            }
            if !left {
                self.draining.remove(&repository);
            }
        }
    }
}
