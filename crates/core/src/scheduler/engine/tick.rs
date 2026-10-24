//! One tick: collects finished work, polls GitHub for runners then queued jobs, plans with `scheduler::plan` and
//! applies the decisions. It owns the memory budget's arithmetic and deletes this machine's runners no slot
//! holds; starting and removing a slot's runner is `runners`. It lists every repository at once, a few requests at
//! a time, and answers the API's commands while it waits for the listings (ADR 0026).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::StreamExt;
use tracing::{info, warn};

use super::admission::Admitting;
use super::{Engine, Occupant, Purpose, RunningJobs, Shutdown};
use crate::api::Basis;
use crate::config::Admission;
use crate::docker::DockerApi;
use crate::github::{self, Deleted, GithubApi, QueuedJob, Runner};
use crate::observe::metrics::SAMPLE_INTERVAL;
use crate::scheduler::{
    Budget, Decision, Plan, Preparing, Profile, Repository, RunnerSlot, Slot, SlotRef, plan,
};
use crate::time;

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
        self.collect_disk().await;
        self.collect_recordings().await;
        self.collect_finished().await;
        self.collect_shells().await;
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
            self.refresh_warm().await;
            self.note_queued();
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
            self.refresh_predictor().await;
            self.rereserve().await;
        }
        let (Plan { decisions, .. }, mut admitting) = match mode {
            Shutdown::No => self.plan(),
            Shutdown::Graceful | Shutdown::Force => (
                Plan {
                    decisions: self
                        .occupied()
                        .filter(|(_, occupant)| mode == Shutdown::Force || !occupant.busy)
                        .map(|(slot, _)| Decision::Stop { slot })
                        .collect(),
                    waiting: BTreeMap::new(),
                    warm: BTreeSet::new(),
                },
                self.admitting(&mut [], None),
            ),
        };
        self.note_high_water(&admitting);
        let mut held_for_cpu = BTreeSet::new();
        // Once a start waits for a warm runner that took a job, the plan's later starts counted on its room too.
        let mut deferred = false;
        for decision in decisions {
            match decision {
                Decision::Stop { slot } => {
                    let jobs = match mode {
                        Shutdown::Force => RunningJobs::Kill,
                        Shutdown::No | Shutdown::Graceful => RunningJobs::Keep,
                    };
                    self.stop(&slot, jobs).await;
                }
                Decision::Start { .. } if deferred => {}
                start @ Decision::Start { .. } => {
                    deferred = !self.apply_start(start, &mut admitting).await;
                }
                Decision::HeldForCpu { repository } => {
                    held_for_cpu.insert(repository);
                }
            }
        }
        self.note_held_for_cpu(&held_for_cpu);
        // Whether a job waits for the host's memory once this plan's starts took theirs, as the status says it.
        match mode {
            Shutdown::No if admitting.host.is_some() => {
                let (after, admitting) = self.plan();
                self.note_host(&admitting, &after.waiting);
            }
            _ => self.note_host(&admitting, &BTreeMap::new()),
        }
        self.idle_caches(mode).await;
        if self.records.listed {
            self.expire_records().await;
        } else {
            self.list_records().await;
        }
        self.prune();
    }

    /// Carries out a plan's `Decision::Start`, unless a warm runner it replaces is still there: that one took a job
    /// before its stop, and the start waits for the next plan. Returns whether it went ahead.
    async fn apply_start(&mut self, start: Decision, admitting: &mut Admitting) -> bool {
        let Decision::Start {
            slot,
            repository,
            memory,
            cpu,
            warm,
            replacing,
        } = start
        else {
            return true;
        };
        if replacing.iter().any(|slot| self.holds_runner(slot)) {
            info!("{repository}: a warm runner it would replace took a job; starting next tick");
            return false;
        }
        let mut admission = admitting.admit(&repository, memory, cpu);
        if warm {
            // Its profile's caps, whatever its repository's queued jobs predict.
            admission.basis = Basis::Cap;
            if let Some(cpu) = admission.cpu.as_mut() {
                cpu.basis = Basis::Cap;
            }
            self.start(&slot, &repository, admission, Purpose::Warm)
                .await;
            return true;
        }
        let held_since = self
            .repositories
            .get_mut(&repository)
            .and_then(|state| state.cpu_wait.take())
            .map(|wait| wait.since);
        if let Some((cpu, since)) = admission.cpu.as_mut().zip(held_since) {
            cpu.waited = Some(since.elapsed().unwrap_or_default().as_secs());
        }
        self.start(&slot, &repository, admission, Purpose::Job)
            .await;
        true
    }

    pub(super) fn plan(&self) -> (Plan, Admitting) {
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        let warm = self.warm_targets();
        let mut repositories: Vec<Repository> = self
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
                    held: state.held,
                    // GitHub lists a job queued for a few seconds after a runner took it: without this, a tick in
                    // between starts a second runner for it.
                    queued: state
                        .queued
                        .iter()
                        .filter(|job| !taken.contains(&job.id))
                        .cloned()
                        .collect(),
                    runner_memory: None,
                    runner_cpu: None,
                    cpu_waiting_since: state.cpu_wait.map(|wait| wait.since),
                    warm: warm.get(&repository.name).copied().unwrap_or(0),
                }
            })
            .collect();
        let profiles = self
            .config
            .profiles
            .iter()
            .map(|(name, profile)| (name.clone(), self.plan_profile(name, profile)))
            .collect();
        let free_memory = self
            .memory_max
            .map(|max| max.saturating_sub(self.memory_in_use()));
        let admitting = self.admitting(&mut repositories, free_memory);
        let mut plan = plan(
            &repositories,
            &profiles,
            Budget {
                memory: admitting.free,
                cpu: admitting.cpu.as_ref().map(|cpu| cpu.free),
                host: admitting.host.map(|host| host.spare),
                held: admitting.held || admitting.host.is_some_and(|host| host.stalled),
            },
            SystemTime::now(),
        );
        // Waiting runners a reload left outside the config: their repository is gone or uses another profile now,
        // or its runners start with other labels, image or limits, which a job queued since may need.
        plan.decisions.extend(
            self.occupied()
                .filter(|(slot, occupant)| {
                    !occupant.busy
                        && occupant.job.is_none()
                        && !self.config.repositories.iter().any(|repository| {
                            repository.name == occupant.repository
                                && repository.profile == slot.profile
                                && self
                                    .config
                                    .profiles
                                    .get(&slot.profile)
                                    .is_some_and(|profile| !occupant.outdated(repository, profile))
                        })
                })
                .map(|(slot, _)| Decision::Stop { slot }),
        );
        let stops = self.over_budget(&plan.decisions);
        plan.decisions.extend(stops);
        (plan, admitting)
    }

    /// A profile as `plan` sees it.
    fn plan_profile(&self, name: &str, profile: &crate::config::Profile) -> Profile {
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
                    memory: self.takes(occupant),
                    cpu: self.reserves_cpu(occupant),
                }),
            })
            .collect();
        Profile {
            count: profile.slots as usize,
            memory: profile.memory.0,
            cpus: (u64::from(profile.cpus) * 1000).min(self.cpu_budget().unwrap_or(u64::MAX)),
            upkeep_due: self.upkeep_waits_for_warm(name),
            preparing: if self.is_reseeding(name) {
                Some(Preparing::Reseeding)
            } else if self.awaits_image(&profile.image) {
                Some(Preparing::PullingImage)
            } else {
                None
            },
            slots,
        }
    }

    /// Whether a slot holds a runner.
    fn holds_runner(&self, slot: &SlotRef) -> bool {
        self.slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .is_some_and(Option::is_some)
    }

    /// What the runners take (`takes`), and the memory
    /// caps of the seed containers (which run with their profile's limits), the shells and the mirror.
    pub(super) fn memory_in_use(&self) -> u64 {
        let runners: u64 = self
            .occupied()
            .map(|(_, occupant)| self.takes(occupant))
            .sum();
        let reseeds: u64 = self.reseed_memory.values().sum();
        runners + reseeds + self.shell_memory() + self.mirror_memory_in_use()
    }

    /// What a runner takes of the budget.
    pub(super) fn takes(&self, occupant: &Occupant) -> u64 {
        match self.config.host.admission {
            // Whatever it reserved under measured admission before a reload.
            Admission::Caps => occupant.memory,
            // What it uses when that is more: a job past its prediction, or one in a burst of starts. Its cap when
            // that is not known: the slice's or its own last sample is older than three intervals, as when the
            // sampler stalls or cannot read its container.
            Admission::Measured => {
                let now = time::now_ms();
                let fresh = 3 * u64::try_from(SAMPLE_INTERVAL.as_millis()).unwrap_or(u64::MAX);
                let slice_sampled = self
                    .store
                    .latest_host_sample()
                    .is_some_and(|sample| sample.at.saturating_mul(1000) + fresh >= now);
                let own = self.store.latest_sample(&occupant.runner_name);
                // Before its first sample, a runner is as fresh as its start.
                let sampled_at = own
                    .as_ref()
                    .map_or_else(|| time::ms(occupant.started_at), |sample| sample.at);
                if !slice_sampled || sampled_at + fresh < now {
                    occupant.memory
                } else {
                    occupant
                        .reserved()
                        .max(own.map_or(0, |sample| sample.memory))
                }
            }
        }
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
