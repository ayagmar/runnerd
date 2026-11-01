//! One tick: collects finished work, polls GitHub for runners then queued jobs, plans with `scheduler::plan` and
//! applies the decisions. It owns the memory budget's arithmetic and deletes this machine's runners no slot
//! holds; starting and removing a slot's runner is `runners`. It lists every repository at once, a few requests at
//! a time, and answers the API's commands while it waits for the listings (ADR 0026).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::StreamExt;
use tracing::{debug, info, warn};

use super::admission::Admitting;
use super::{Engine, Occupant, Purpose, Shutdown};
use crate::admission::host;
use crate::api::{AdmissionPolicy, Basis};
use crate::config::Admission;
use crate::docker::DockerApi;
use crate::github::{self, Deleted, GithubApi, QueuedJob, Runner};
use crate::observe::metrics::SAMPLE_INTERVAL;
use crate::scheduler::{
    Budget, Decision, Left, Plan, Pool, Preparing, Profile, Repository, RunnerSlot, Slot, SlotRef,
    StopReason, plan,
};
use crate::store::Store;
use crate::time;

/// GitHub requests a tick has in flight at most. GitHub asks clients not to flood it with concurrent requests;
/// a few keep a tick over many repositories short.
pub(super) const CONCURRENT_REQUESTS: usize = 4;

/// Plans a tick makes at most: its first, and those after starts that no longer fit what ran by then.
const MAX_PLANS: usize = 3;

/// What carrying out one plan did.
#[derive(Debug, Default)]
struct Applied {
    /// Whether a runner was removed or started.
    acted: bool,
    /// Starts left for the next plan: the one that no longer fit, and those after it.
    deferred: usize,
    /// The pools whose next job a slot and memory had room for, the CPU budget not.
    held_for_cpu: BTreeSet<Pool>,
}

/// A part of a tick, as its pacing log line names it.
#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Collecting ended runners, finished reseeds, recordings and checks.
    Collecting,
    /// GitHub's runners and queued jobs listings.
    Listing,
    /// Applying the listings, following running jobs, pulls, predictions and reservations.
    Following,
    /// Removing idle runners.
    Removing,
    /// Starting runners.
    Starting,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Collecting => "collecting",
            Phase::Listing => "listing",
            Phase::Following => "following",
            Phase::Removing => "removing",
            Phase::Starting => "starting",
        }
    }
}

/// How long a tick took to reach its last start, by phase (ADR 0092): what a job listed in it waited for on
/// runnerd's side before its runner's container started.
struct Pace {
    began: tokio::time::Instant,
    last: tokio::time::Instant,
    phases: Vec<(Phase, std::time::Duration)>,
    removed: usize,
    started: usize,
}

impl Pace {
    fn new() -> Pace {
        let now = tokio::time::Instant::now();
        Pace {
            began: now,
            last: now,
            phases: Vec::new(),
            removed: 0,
            started: 0,
        }
    }

    /// Ends `phase`, which ran since the last one ended.
    fn lap(&mut self, phase: Phase) {
        let now = tokio::time::Instant::now();
        let took = now - self.last;
        self.last = now;
        match self
            .phases
            .iter_mut()
            .find(|(seen, _)| seen.name() == phase.name())
        {
            Some((_, total)) => *total += took,
            None => self.phases.push((phase, took)),
        }
    }

    /// Logs the tick's pace after `plans` plans: at debug level, or as info when it took longer than a poll
    /// `interval` to reach its last start, which a job listed in it waited through on runnerd's side.
    fn done(&self, interval: std::time::Duration, plans: usize) {
        let took = self.last - self.began;
        let phases = self
            .phases
            .iter()
            .map(|(phase, took)| format!("{} {}", phase.name(), seconds(*took)))
            .collect::<Vec<_>>()
            .join(", ");
        let line = format!(
            "tick reached its last start after {}: {phases}; {} removals, {} starts, {plans} plans",
            seconds(took),
            self.removed,
            self.started
        );
        if took > interval {
            info!("{line}");
        } else {
            debug!("{line}");
        }
    }
}

/// A phase's length for the pacing log line, to a tenth of a second.
fn seconds(took: std::time::Duration) -> String {
    format!("{:.1}s", took.as_secs_f64())
}

/// One answer of GitHub's per repository.
type Answers<T> = Vec<(String, Result<T, github::Error>)>;

/// What a tick's GitHub listings returned, by repository.
struct Listings {
    runners: Answers<Vec<Runner>>,
    queued: Vec<Listed>,
}

/// A repository's queued jobs as GitHub answered them, and those of them the history recorded.
struct Listed {
    repository: String,
    answer: Result<Vec<QueuedJob>, github::Error>,
    recorded: Vec<u64>,
}

/// A repository to list the queued jobs of, with the labels of each of its pools.
struct ToList {
    repository: String,
    pools: Vec<Vec<String>>,
}

/// Lists the runners of `runners`, then the queued jobs of `queued`, `CONCURRENT_REQUESTS` at a time. Owns what it
/// needs, so the engine answers commands while it runs. The history keeps each queued job a pool's labels match as
/// seen the moment its listing answers (ADR 0092): a runner may take it, or the tick end, before the others answer.
async fn list<G: GithubApi>(
    github: Arc<G>,
    store: Store,
    runners: Vec<String>,
    queued: Vec<ToList>,
) -> Listings {
    let (github, store) = (&*github, &store);
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
        .map(|ToList { repository, pools }| async move {
            let answer = github.queued_jobs(&repository).await;
            let at = time::now_ms();
            let mut recorded = Vec::new();
            for job in answer.iter().flatten() {
                if pools
                    .iter()
                    .any(|labels| crate::scheduler::matches(job, labels))
                {
                    store.queued(&repository, job, at);
                    recorded.push(job.id);
                }
            }
            Listed {
                repository,
                answer,
                recorded,
            }
        })
        .buffered(CONCURRENT_REQUESTS)
        .collect()
        .await;
    Listings { runners, queued }
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    pub(super) async fn tick(&mut self, mode: Shutdown) {
        let mut pace = Pace::new();
        self.collect_reseeds().await;
        self.collect_measures().await;
        self.collect_trim().await;
        self.collect_disk().await;
        self.collect_recordings().await;
        // Before the finished runners go: one Docker never inspected while it ran is still recorded with its job.
        self.observe_again().await;
        self.recapture().await;
        self.collect_finished().await;
        self.collect_shells().await;
        self.kill_cancelled().await;
        pace.lap(Phase::Collecting);
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
            Shutdown::No => self.to_list(),
            Shutdown::Graceful | Shutdown::Detach | Shutdown::Force => Vec::new(),
        };
        let listings = self
            .serving(list(
                self.github.clone(),
                self.store.clone(),
                runners,
                queued,
            ))
            .await;
        pace.lap(Phase::Listing);
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
            self.follow_visibility();
            self.pull_images().await;
            self.refresh_predictor().await;
            self.rereserve().await;
        }
        pace.lap(Phase::Following);
        let (plan, mut admitting) = match mode {
            Shutdown::No => self.plan(),
            Shutdown::Graceful | Shutdown::Detach | Shutdown::Force => {
                (self.stops(mode), self.admitting(&mut [], None))
            }
        };
        self.note_high_water(&admitting);
        self.note_slots(plan.slot_waiting.clone());
        let mut applied = self.apply(plan, &mut admitting, mode, &mut pace).await;
        // A start that no longer fits what runs now is decided again at once, from the same listings, rather than
        // holding back every later start of the plan until the next poll. A plan again only after one that did
        // something, so a start that keeps not fitting costs one more plan, not a loop. Not once a stop is asked for.
        let mut plans = 1;
        while self.stopping() == Shutdown::No
            && applied.deferred > 0
            && applied.acted
            && plans < MAX_PLANS
        {
            let (plan, again) = self.plan();
            admitting = again;
            plans += 1;
            applied = self.apply(plan, &mut admitting, mode, &mut pace).await;
        }
        if applied.deferred > 0 {
            info!(
                "{} of this tick's starts no longer fit what runs now; starting them next tick",
                applied.deferred
            );
        }
        // The last plan's: a pool an earlier plan held may have started since.
        self.note_held_for_cpu(&applied.held_for_cpu);
        pace.done(self.config.host.poll_interval, plans);
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

    /// Each configured repository with the labels of its pools, whose queued jobs a tick lists.
    fn to_list(&self) -> Vec<ToList> {
        self.config
            .repository_names()
            .into_iter()
            .map(|repository| ToList {
                pools: self
                    .config
                    .repositories
                    .iter()
                    .filter(|pool| pool.name == repository)
                    .map(|pool| pool.labels.clone())
                    .collect(),
                repository,
            })
            .collect()
    }

    /// Carries out `plan`: its stops first and all at once (`remove_idle`), as they only give room back and the starts
    /// that take it come after them in the plan anyway, then its starts one at a time, each admitted against what runs
    /// by then. Once a start is deferred, the plan's later starts counted on its room too and wait for the next plan.
    /// None starts once a stop is asked for, even mid-plan.
    async fn apply(
        &mut self,
        plan: Plan,
        admitting: &mut Admitting,
        mode: Shutdown,
        pace: &mut Pace,
    ) -> Applied {
        let mut applied = Applied::default();
        let (stops, decisions): (Vec<Decision>, Vec<Decision>) = plan
            .decisions
            .into_iter()
            .partition(|decision| matches!(decision, Decision::Stop { .. }));
        let stops: Vec<(SlotRef, StopReason)> = stops
            .into_iter()
            .filter_map(|decision| match decision {
                Decision::Stop { slot, reason } => Some((slot, reason)),
                Decision::Start { .. } | Decision::HeldForCpu { .. } => None,
            })
            .collect();
        let before = self.occupied().count();
        pace.removed += stops.len();
        if mode == Shutdown::Force {
            for (slot, _) in stops {
                self.kill(&slot).await;
            }
        } else {
            self.remove_idle(stops).await;
        }
        applied.acted = self.occupied().count() < before;
        pace.lap(Phase::Removing);
        for decision in decisions {
            match decision {
                Decision::Stop { .. } => {}
                // A runner started after a stop was asked for could take a job a detaching stop leaves running.
                Decision::Start { .. } if self.stopping() != Shutdown::No => {}
                Decision::Start { .. } if applied.deferred > 0 => applied.deferred += 1,
                start @ Decision::Start { .. } => {
                    if self.apply_start(start, admitting).await {
                        applied.acted = true;
                        pace.started += 1;
                    } else {
                        applied.deferred += 1;
                    }
                }
                Decision::HeldForCpu {
                    repository,
                    profile,
                } => {
                    applied.held_for_cpu.insert(Pool {
                        repository,
                        profile,
                    });
                }
            }
        }
        pace.lap(Phase::Starting);
        applied
    }

    /// What a stop in `mode` removes: every runner when forced, the idle ones when graceful, none when it detaches,
    /// which leaves them to the next start.
    fn stops(&self, mode: Shutdown) -> Plan {
        Plan {
            decisions: self
                .occupied()
                .filter(|(_, occupant)| {
                    mode == Shutdown::Force || mode == Shutdown::Graceful && !occupant.busy
                })
                .map(|(slot, _)| Decision::Stop {
                    slot,
                    reason: StopReason::Shutdown,
                })
                .collect(),
            ..Plan::default()
        }
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
            info!("{repository}: a warm runner it would replace took a job; planning again");
            return false;
        }
        // The plan's budgets are those of its start: the starts before this one are counted since, and so are what
        // ran meanwhile.
        if !self.still_fits(admitting, &slot.profile, memory, cpu) {
            info!(
                "{repository}: its runner in {} no longer fits what runs now; planning again",
                slot.profile
            );
            return false;
        }
        let pool = Pool {
            repository,
            profile: slot.profile.clone(),
        };
        self.refresh_free(admitting);
        let mut admission = admitting.admit(&pool, memory, cpu);
        let repository = pool.repository;
        if warm {
            // Its profile's caps, whatever its repository's queued jobs predict.
            admission.basis = Basis::Cap;
            if let Some(cpu) = admission.cpu.as_mut() {
                cpu.basis = Basis::Cap;
            }
            let purpose = self.warm_purpose(&Pool {
                repository: repository.clone(),
                profile: slot.profile.clone(),
            });
            self.start(&slot, &repository, admission, purpose).await;
            return true;
        }
        let held_since = self
            .repositories
            .get_mut(&repository)
            .and_then(|state| state.cpu_wait.remove(&slot.profile))
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
                    held: state
                        .holds
                        .get(&repository.profile)
                        .and_then(|hold| hold.held),
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
                    cpu_waiting_since: state
                        .cpu_wait
                        .get(&repository.profile)
                        .map(|wait| wait.since),
                    warm: warm.get(&Pool::from(repository)).copied().unwrap_or(0),
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
            self.plan_budget(&admitting),
            SystemTime::now(),
        );
        // Waiting runners a reload left outside the config: their repository is gone or uses another profile now,
        // or its runners start with other labels, image or limits, which a job queued since may need. Also those an
        // earlier release started, whose container spec this one may have changed.
        plan.decisions.extend(
            self.occupied()
                .filter(|(slot, occupant)| {
                    !occupant.busy
                        && occupant.job.is_none()
                        && (occupant.other_spec()
                            || !self.config.repositories.iter().any(|repository| {
                                repository.name == occupant.repository
                                    && repository.profile == slot.profile
                                    && self.config.profiles.get(&slot.profile).is_some_and(
                                        |profile| {
                                            // The old image still runs jobs: kept until the new one is present.
                                            !occupant.outdated(repository, profile)
                                                && (!occupant.other_image(profile)
                                                    || self.awaits_image(&profile.image))
                                        },
                                    )
                            }))
                })
                .map(|(slot, _)| Decision::Stop {
                    slot,
                    reason: StopReason::Changed,
                }),
        );
        let stops = self.over_budget(&plan.decisions);
        plan.decisions.extend(stops);
        (plan, admitting)
    }

    /// The budgets a plan has beside what runs now, as `admitting` admits.
    fn plan_budget(&self, admitting: &Admitting) -> Budget {
        Budget {
            // Signed, as the host's: a warm runner's room pays back what runs past the budget first.
            memory: if admitting.held {
                Some(0)
            } else {
                self.memory_max
                    .map(|max| crate::scheduler::left(max, self.memory_in_use()))
            },
            cpu: admitting
                .cpu
                .as_ref()
                .and(self.cpu_budget())
                .map(|budget| crate::scheduler::left(budget, self.cpu_in_use())),
            host: admitting
                .host
                .as_ref()
                .map(|host| host::left(host.limit, self.memory_in_use(), host.stalled)),
            held: admitting.held || admitting.host.as_ref().is_some_and(|host| host.stalled),
        }
    }

    /// Whether a runner of `memory` and `cpu` that `plan` started in `profile` still fits, by the plan's rule, the
    /// budgets as they are now: the samples and host readings taken since, and what ran past what the plan counted
    /// on, such as a runner whose stop failed. Without a recent sample of the slice, measured admission counts it at
    /// its cap, as the plan would then. The host keeps the margin the plan chose: the runners a plan starts into an
    /// empty slice all need only the first runner's.
    fn still_fits(&self, plan: &Admitting, profile: &str, memory: u64, cpu: u64) -> bool {
        let mut now = self.admitting(&mut [], None);
        now.host = self.host_budget_by(plan.remembered);
        let memory = match self.config.profiles.get(profile) {
            Some(profile) if now.policy == AdmissionPolicy::Measured && now.slice.is_none() => {
                memory.max(profile.memory.0)
            }
            _ => memory,
        };
        let budget = self.plan_budget(&now);
        let left = Left {
            memory: budget.memory,
            cpu: budget.cpu,
            host: budget.host,
        };
        !budget.held && left.fits(memory, cpu)
    }

    /// A profile as `plan` sees it.
    fn plan_profile(&self, name: &str, profile: &crate::config::Profile) -> Profile {
        let slots = &self.slots[name];
        // A reload may have left fewer slots than runners: free slots count only up to the configured number.
        let mut free = (profile.slots as usize)
            .saturating_sub(slots.iter().filter(|slot| slot.is_some()).count());
        let slots_len = slots.len();
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
            ran: (0..slots_len)
                .map(|index| {
                    self.ran
                        .get(&SlotRef {
                            profile: name.to_owned(),
                            index,
                        })
                        .cloned()
                })
                .collect(),
        }
    }

    /// Whether a slot holds a runner.
    fn holds_runner(&self, slot: &SlotRef) -> bool {
        self.slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .is_some_and(Option::is_some)
    }

    /// What the runners take (`takes`), and the memory caps of the seed containers (which run with their profile's
    /// limits) and the room waiting ones hold (`seed_memory`), the shells and the mirror.
    pub(super) fn memory_in_use(&self) -> u64 {
        let runners: u64 = self
            .occupied()
            .map(|(_, occupant)| self.takes(occupant))
            .sum();
        let reseeds: u64 = self.seed_memory().values().sum();
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

    /// Takes each repository's queued jobs from its listing. A repository a command removed meanwhile is skipped, but
    /// the jobs its listing recorded are followed until GitHub says what became of them.
    fn apply_queued(&mut self, listings: Vec<Listed>) {
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        for Listed {
            repository,
            answer,
            recorded,
        } in listings
        {
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
            } else {
                for job in recorded.into_iter().filter(|job| !taken.contains(job)) {
                    self.queued_jobs
                        .entry(job)
                        .or_insert_with(|| repository.clone());
                }
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
            if left || !self.draining.contains(&repository) {
                self.drained_once.remove(&repository);
            } else if !self.drained_once.insert(repository.clone()) {
                self.drained_once.remove(&repository);
                self.draining.remove(&repository);
            }
        }
    }
}
