//! One tick: collects finished work, polls GitHub for runners then queued jobs, plans with `scheduler::plan` and
//! applies the decisions. It owns the memory budget's arithmetic and deletes this machine's runners no slot
//! holds; starting and removing a slot's runner is `runners`.

use std::collections::BTreeSet;
use std::time::SystemTime;

use tracing::{info, warn};

use super::{Engine, Shutdown};
use crate::docker::DockerApi;
use crate::github::{self, Deleted, GithubApi};
use crate::scheduler::{Decision, Profile, Repository, RunnerSlot, Slot, plan};

impl<G: GithubApi, D: DockerApi + 'static> Engine<G, D> {
    pub(super) async fn tick(&mut self, mode: Shutdown) {
        self.collect_reseeds().await;
        self.collect_trim().await;
        self.collect_finished().await;
        self.kill_cancelled().await;
        // Runners first: a runner that takes its job after this poll still counts as idle against a job the later
        // queued-jobs poll no longer lists, which leaves it surplus at worst (it stops after IDLE_TIMEOUT). The other
        // order sees the job queued and its runner busy, and starts a second runner for it.
        self.poll_runners().await;
        if mode == Shutdown::No {
            self.poll_queued().await;
        } else {
            for state in self.repositories.values_mut() {
                state.listed = false;
            }
        }
        self.take_found();
        self.follow_jobs().await;
        self.count_mirror_bytes().await;
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
                Decision::Stop { slot } => self.stop(&slot, mode == Shutdown::Force).await,
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
                    reseeding: self.is_reseeding(name),
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
        // The configured mirror is reserved for; a running one keeps the cap it started with until it stops.
        let mirror = &self.config.caches.mirror;
        let configured = if mirror.enabled { mirror.memory.0 } else { 0 };
        let running = if self.mirror_up {
            self.mirror_memory
        } else {
            0
        };
        let runners: u64 = self.occupied().map(|(_, occupant)| occupant.memory).sum();
        let reseeds: u64 = self.reseed_memory.values().sum();
        runners + reseeds + configured.max(running)
    }

    async fn poll_queued(&mut self) {
        for repository in &self.config.repositories {
            let mut listed = false;
            let queued = match self.github.queued_jobs(&repository.name).await {
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
                    warn!("{}: cannot list queued jobs: {error}", repository.name);
                    Vec::new()
                }
            };
            if let Some(state) = self.repositories.get_mut(&repository.name) {
                state.queued = queued;
                state.listed = listed;
            }
        }
    }

    /// Updates each runner's busy flag and deletes this host's runners that no slot holds.
    async fn poll_runners(&mut self) {
        let names: Vec<String> = self
            .repositories
            .keys()
            .chain(&self.draining)
            .cloned()
            .collect();
        for repository in names {
            // Whether this machine still has a runner registered on it.
            let mut left = false;
            let runners = match self.github.runners(&repository).await {
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
