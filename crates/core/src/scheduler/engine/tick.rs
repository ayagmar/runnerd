//! One tick: collects finished work, polls GitHub for runners then queued jobs, plans with `scheduler::plan` and
//! applies the decisions; and the passes between ticks that start runners for listed jobs in the slots of runners
//! Docker said stopped, without polling (ADR 0095). It owns the memory budget's arithmetic, which repositories a tick
//! lists and when the next is due (ADR 0128), and deletes this machine's runners no slot holds; starting and removing a
//! slot's runner is `runners`. It lists the repositories due at once, a few requests at a time, and answers the API's
//! commands while it waits for the listings (ADR 0026).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::StreamExt;
use tracing::{debug, info, warn};

use super::admission::Admitting;
use super::engagement::Mark;
use super::history::CAPTURE_DRAIN;
use super::listing_pace::Runners;
use super::pool_health::PoolHealth;
use super::presence::RunnersListing;
use super::runners::{Collected, Removal};
use super::slot::Holder;
use super::start::{Carried, StartOutcome};
use super::{Engine, Footprint, Meanwhile, Occupant, Shutdown, TICK_REST};
use crate::admission::trace::Reading;
use crate::admission::{host, pooled};
use crate::api::AdmissionPolicy;
use crate::config::Admission;
use crate::docker::{self, DockerApi};
use crate::github::{self, Deleted, GithubApi, QueuedJob, Runner};
use crate::observe::metrics::SAMPLE_INTERVAL;
use crate::scheduler::{
    Barrier, Budget, Decision, Demand, Drained, Left, Plan, Pool, Preparing, Profile, Repository,
    RunnerSlot, Slot, SlotRef, StartFor, StopReason, plan,
};
use crate::store::Store;
use crate::time;

/// GitHub requests a tick has in flight at most. GitHub asks clients not to flood it with concurrent requests;
/// a few keep a tick over many repositories short.
pub(super) const CONCURRENT_REQUESTS: usize = 4;

/// Plans a tick makes at most: its first, and those after starts that no longer fit what ran by then.
const MAX_PLANS: usize = 3;

/// A runner a pass releases, and its log once read.
struct Ending {
    slot: SlotRef,
    occupant: Occupant,
    drained: Collected,
}

/// The releases a pass began, by index: each `None` once done.
#[derive(Default)]
struct Releases {
    ended: Vec<Option<Ending>>,
    /// The step each waits for.
    waits: tokio::task::JoinSet<(usize, Step)>,
}

impl Releases {
    /// Whether the pass is releasing the runner of `container`.
    fn releasing(&self, container: &str) -> bool {
        self.ended
            .iter()
            .flatten()
            .any(|ending| ending.occupant.container == container)
    }
}

/// A step of a release a pass waited for.
enum Step {
    /// Its log read, and its capture ended.
    Drained(Collected),
    /// Its container removed and its registration deleted, or why Docker did not remove it.
    Removed(Result<Removal, docker::Error>),
}

/// What carrying out one plan did.
#[derive(Debug, Default)]
struct Applied {
    /// Whether a runner was removed or started.
    acted: bool,
    /// Starts left for the next plan: the one that no longer fit, and those after it.
    deferred: usize,
    /// The next jobs of the pools a slot had room for, memory or CPU not.
    held: Vec<Demand>,
    /// The job no younger job started before.
    barrier: Option<Barrier>,
}

/// A part of a tick, as its pacing log line names it.
#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Collecting ended runners, finished reseeds, recordings and checks.
    Collecting,
    /// GitHub's runners and queued jobs listings.
    Listing,
    /// Applying the listings, following the jobs the plan needs, pulls, predictions and reservations.
    Following,
    /// The passes the tick hosted while it waited for GitHub (ADR 0095), which start runners of their own.
    Passes,
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
            Phase::Passes => "passes",
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
    /// What the passes it hosted took, as of the last lap.
    passes: std::time::Duration,
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
            passes: std::time::Duration::ZERO,
        }
    }

    /// Ends `phase`, which ran since the last one ended, with the passes the tick hosted taking `passes` so far: what
    /// they took since is theirs, not the phase's.
    fn lap(&mut self, phase: Phase, passes: std::time::Duration) {
        let now = tokio::time::Instant::now();
        let hosted = passes.saturating_sub(self.passes);
        self.passes = passes;
        let took = (now - self.last).saturating_sub(hosted);
        self.last = now;
        self.add(phase, took);
        if !hosted.is_zero() {
            self.add(Phase::Passes, hosted);
        }
    }

    /// Adds `took` to `phase`.
    fn add(&mut self, phase: Phase, took: std::time::Duration) {
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
pub(super) fn seconds(took: std::time::Duration) -> String {
    format!("{:.1}s", took.as_secs_f64())
}

/// Which repositories a tick lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// Every repository, with its runners: a tick a command, a wake-up or a stop brings, and the first.
    Every,
    /// Those due by their own pace, their runners on their turn (ADR 0128): a tick that came when it was due.
    Due,
}

/// What a tick lists.
struct Probes {
    /// The repositories whose runners it lists: the listed ones by name, then the draining ones.
    runners: Vec<String>,
    /// The repositories whose queued jobs it lists, with their pools' labels.
    queued: Vec<ToList>,
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

/// What a runner holds of the budget.
struct Held {
    /// What it takes of it.
    takes: u64,
    /// What it uses, from a recent sample.
    used: Reading,
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// The next tick, as the tests count ticks: a `poll_interval` after the last, listing every repository and its
    /// runners. Its listings end the claims a listing that much later ends (ADR 0128).
    #[cfg(test)]
    pub(super) async fn tick(&mut self, mode: Shutdown) -> tokio::time::Instant {
        self.age_listings(self.config.host.poll_interval);
        self.tick_listing(mode, Scope::Every).await
    }

    /// As if every listing had begun, and every claim on a listed job been made, `by` earlier: the repositories'
    /// due times, the newly busy runners' marks and the unnamed claims of runners gone (ADR 0128).
    #[cfg(test)]
    pub(super) fn age_listings(&mut self, by: std::time::Duration) {
        for (_, state) in self.repositories.iter_listed_mut() {
            state.pace_mut().age(by);
        }
        for occupant in self
            .slots
            .values_mut()
            .flatten()
            .filter_map(super::Slot::occupant_mut)
        {
            occupant.engagement.age_mark(by);
        }
        self.claims.age(by);
        if let Some(at) = self.listing.at.checked_sub(by) {
            self.listing.at = at;
        }
    }

    /// One tick: collects what finished, lists the runners and queued jobs of the repositories `scope` says, follows
    /// the running jobs, plans and applies the plan. Returns when its listing began, which the due times count from
    /// (ADR 0111, 0128).
    pub(super) async fn tick_listing(
        &mut self,
        mode: Shutdown,
        scope: Scope,
    ) -> tokio::time::Instant {
        let began = SystemTime::now();
        let mut pace = Pace::new();
        self.passes_in_tick = std::time::Duration::ZERO;
        self.pace_by_limit();
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
        pace.lap(Phase::Collecting, self.passes_in_tick);
        // Runners first: a runner that takes its job after this poll still counts as idle against a job the later
        // queued-jobs poll no longer lists, which leaves it surplus at worst (it stops after IDLE_TIMEOUT). The other
        // order sees the job queued and its runner busy, and starts a second runner for it.
        let listed_at = tokio::time::Instant::now();
        let Probes { runners, queued } = self.probes(mode, scope, listed_at);
        // What a runner takes from here on, this listing may still show queued.
        self.listing = Mark {
            listing: self.listing.listing + 1,
            at: listed_at,
        };
        let listing = self.listing;
        let listings = self
            .serving(
                list(self.github.clone(), self.store.clone(), runners, queued),
                Meanwhile::Turnover,
            )
            .await;
        pace.lap(Phase::Listing, self.passes_in_tick);
        self.apply_runners(listings.runners, listing).await;
        if mode == Shutdown::No {
            self.apply_queued(listings.queued, listing);
            self.refresh_warm().await;
            self.note_queued();
        } else {
            for (_, state) in self.repositories.iter_listed_mut() {
                state.listing_mut().stop();
            }
        }
        self.forget_unlisted();
        self.take_found();
        let described = self.follow_jobs().await;
        self.count_mirror_bytes().await;
        if mode == Shutdown::No {
            self.follow_visibility();
            self.pull_images().await;
            self.refresh_predictor().await;
            self.rereserve().await;
        }
        pace.lap(Phase::Following, self.passes_in_tick);
        let (plan, mut admitting) = match mode {
            Shutdown::No => self.plan(),
            Shutdown::Graceful | Shutdown::Detach | Shutdown::Force => {
                (self.stops(mode), self.admitting(&mut [], None))
            }
        };
        self.note_high_water(&admitting);
        let mut applied = self.apply(plan, &mut admitting, mode, &mut pace).await;
        // A start that no longer fits what runs now is decided again at once, from the same listings, rather than
        // holding back every later start of the plan until the next poll. A plan again only after one that did
        // something, so a start that keeps not fitting costs one more plan, not a loop. Not once a stop is asked for.
        let mut plans = 1;
        while self.control.mode() == Shutdown::No
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
        self.note_waits(&applied.held, applied.barrier);
        pace.done(self.config.host.poll_interval, plans);
        // Whether a job waits for the host's memory once this plan's starts took theirs, as the status says it.
        match mode {
            Shutdown::No if admitting.host.is_some() => {
                let (after, admitting) = self.plan();
                self.note_host(&admitting, &after.waiting);
            }
            _ => self.note_host(&admitting, &BTreeMap::new()),
        }
        // A stop asked during the starts reads the jobs in its own tick, or leaves them to the next start's: its
        // idle runners are removed first (ADR 0091).
        if self.control.mode() == mode {
            self.follow_running(described, listed_at).await;
        }
        self.idle_caches(mode, began).await;
        self.keep_records().await;
        self.prune();
        listed_at
    }

    /// A pass between ticks for the runners Docker said stopped (ADR 0095): releases those whose containers did, as
    /// the tick's `collect_finished` does, and as each one's slot is free starts runners for the jobs the last poll
    /// listed queued into the room left, without asking GitHub again. Each release keeps its steps' order, but their
    /// waits for Docker and GitHub run side by side, and runners Docker says stopped while they wait join them, so a
    /// slow one holds no other slot. The pass owns its releases until each is done: nothing else sees a runner half
    /// released. Only starts for jobs: stops, warm runners and starts that take a warm runner's room wait for the
    /// tick, which also follows jobs and keeps the caches. The listing it plans from is that of the runners before the
    /// queued jobs, as every tick's; jobs runners took since are left out (`listed_demand`).
    pub(super) async fn turnover(&mut self) {
        let began = tokio::time::Instant::now();
        let mut releases = Releases::default();
        let mut freed = self.begin_releases(&mut releases).await;
        if freed == 0 && releases.waits.is_empty() {
            return;
        }
        let mut started = if freed > 0 { self.refill().await } else { 0 };
        let exits = self.exits.clone();
        // Runners that stop later join only for as long as one release may drain: past that, a pass whose slots
        // keep turning over would hold the engine until the backlog drained, no tick nor command between. Those
        // stopping after are the next pass's, which their hint wakes.
        let joining = tokio::time::sleep_until(began + CAPTURE_DRAIN);
        tokio::pin!(joining);
        let mut join = true;
        while !releases.waits.is_empty() {
            tokio::select! {
                () = &mut joining, if join => join = false,
                joined = releases.waits.join_next() => match joined {
                    Some(Ok((index, Step::Drained(read)))) => {
                        let Some(Ending { slot, occupant, drained }) = &mut releases.ended[index] else {
                            continue;
                        };
                        *drained = read;
                        let recorded = self.record(slot, occupant, drained).await;
                        let removed = self.remove(occupant, recorded, drained.logs.as_deref());
                        releases
                            .waits
                            .spawn(async move { (index, Step::Removed(removed.await)) });
                    }
                    Some(Ok((index, Step::Removed(removed)))) => {
                        let Some(Ending { slot, occupant, drained }) = releases.ended[index].take() else {
                            continue;
                        };
                        self.released(&slot, &occupant, drained.logs.as_deref(), removed);
                        if !self.holds_runner(&slot) {
                            freed += 1;
                            started += self.refill().await;
                        }
                    }
                    // A wait that panicked leaves its runner in its slot, which the next tick's inspect finds again.
                    Some(Err(_)) | None => {}
                },
                // Once a stop is asked for, the tick releases what stopped since.
                () = exits.noted(), if join && self.control.mode() == Shutdown::No => {
                    let more = self.begin_releases(&mut releases).await;
                    if more > 0 {
                        freed += more;
                        started += self.refill().await;
                    }
                }
            }
        }
        // At info only when it started one, so the journal shows what woke each such start, one line a turnover.
        let line = format!(
            "pass after runners ended: {freed} slots freed, {started} runners started in {}",
            seconds(began.elapsed())
        );
        if started > 0 {
            info!("{line}");
        } else {
            debug!("{line}");
        }
    }

    /// Takes the runners Docker said stopped since, inspects those not already being released and begins releasing
    /// those that did stop; returns how many of them this tick's inspect released first, in the moment before their
    /// event came, whose room waits for no listing either.
    async fn begin_releases(&mut self, releases: &mut Releases) -> usize {
        let noted = self.exits.take();
        if noted.is_empty() {
            return 0;
        }
        let freed = noted
            .containers
            .iter()
            .filter(|container| self.released.remove(*container))
            .count();
        let noted: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .filter(|(_, occupant)| noted.any || noted.containers.contains(&occupant.container))
            .filter(|(_, occupant)| !releases.releasing(&occupant.container))
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        for (slot, occupant) in noted {
            if self.ended(&occupant).await {
                let drained = self.drain(&occupant);
                let index = releases.ended.len();
                releases.ended.push(Some(Ending {
                    slot,
                    occupant,
                    drained: Collected::default(),
                }));
                releases
                    .waits
                    .spawn(async move { (index, Step::Drained(drained.await)) });
            }
        }
        freed
    }

    /// Starts runners for the listed jobs into the room a pass freed; returns how many started.
    pub(super) async fn refill(&mut self) -> usize {
        if self.control.mode() != Shutdown::No || self.github_limit().is_some() {
            return 0;
        }
        // The jobs captures named since: their runners are busy, and their jobs no longer queued.
        self.take_found();
        // What waiting runners reserve for the jobs listed now, as a tick raises it before its plan: a pass may run
        // after a tick took its listing and before it did.
        self.rereserve().await;
        let (plan, mut admitting) = self.plan();
        // The jobs the plan starts a runner for that this pass did not: they go on waiting for what they waited for.
        let mut not_carried_out: BTreeSet<u64> = plan
            .decisions
            .iter()
            .filter_map(|decision| match decision {
                Decision::Start {
                    for_: StartFor::Job(job),
                    ..
                } => Some(*job),
                _ => None,
            })
            .collect();
        // Only starts for jobs: one that takes a warm runner's room waits for the tick.
        let (jobs, starts): (Vec<u64>, Vec<Decision>) = plan
            .decisions
            .iter()
            .filter_map(|decision| match decision {
                Decision::Start {
                    for_: StartFor::Job(job),
                    replacing,
                    ..
                } if replacing.is_empty() => Some((*job, decision.clone())),
                _ => None,
            })
            .unzip();
        let carried = self.start_batch(starts, &mut admitting).await;
        let mut started = 0;
        for (job, carried) in jobs.iter().zip(carried) {
            if let Carried::Done(outcome) = carried {
                started += usize::from(outcome == StartOutcome::Started);
                not_carried_out.remove(job);
            }
        }
        self.note_pass_waits(&plan, &not_carried_out);
        // A plan's waits count whichever path ran it, and only once its starts are done.
        self.note_waits(&plan.held, plan.barrier);
        started
    }

    /// What a tick in `mode` listing `scope` lists from `at`, each repository it probes noting that it did: in a stop
    /// every repository's runners and no queued jobs; otherwise, with `Scope::Due`, the repositories due and their
    /// runners on their turn, and every draining repository's runners.
    fn probes(&mut self, mode: Shutdown, scope: Scope, at: tokio::time::Instant) -> Probes {
        let (scope, queued) = match mode {
            Shutdown::No => (scope, true),
            Shutdown::Graceful | Shutdown::Detach | Shutdown::Force => (Scope::Every, false),
        };
        let due: BTreeSet<String> = self
            .repositories
            .iter_listed()
            .filter(|(name, state)| {
                scope == Scope::Every || state.pace().is_due(at, self.listing_every(name))
            })
            .map(|(name, _)| name.clone())
            .collect();
        let runners_by = match scope {
            Scope::Every => Runners::Now,
            Scope::Due => Runners::ByTurn,
        };
        let mut runners = Vec::new();
        for name in self.repositories.runners_to_list() {
            let lists = match self.repositories.listed_mut(&name) {
                Some(state) if due.contains(&name) => state.pace_mut().begin(at, runners_by),
                Some(_) => false,
                // Draining: what ends its drain is a runners listing.
                None => true,
            };
            if lists {
                runners.push(name);
            }
        }
        Probes {
            runners,
            queued: if queued {
                self.to_list(&due)
            } else {
                Vec::new()
            },
        }
    }

    /// How long `repository`'s listings are apart: a third of `poll_interval` while it is active, `poll_interval`
    /// while it is idle, each as the governor paces it (ADR 0127, 0128).
    pub(super) fn listing_every(&self, repository: &str) -> std::time::Duration {
        let poll = self.config.host.poll_interval;
        if self.listing_active(repository) {
            self.governor.active_listings_every(poll)
        } else {
            self.governor.listings_every(poll)
        }
    }

    /// Whether `repository` is listed at the active pace: not paused, and active by ADR 0107's rule, which counts a job
    /// its listings show queued while they show it, or a runner of it holds a job, as one past `ACTIVE_AFTER` does.
    /// Only the listings' pace reads this; warm runners go by ADR 0107's rule alone.
    fn listing_active(&self, repository: &str) -> bool {
        !self.control.is_paused(repository)
            && (self.warm.active(repository, time::now_ms())
                || self
                    .occupied()
                    .any(|(_, occupant)| occupant.repository == repository && occupant.engaged()))
    }

    /// When the next tick after one whose listing began at `anchor` is due: at the earliest repository's due time,
    /// and at least `TICK_REST` from now, or the shortest interval if that is shorter (ADR 0111, 0128). With no
    /// repository listed, the idle interval after `anchor`.
    pub(super) fn next_tick_due(&self, anchor: tokio::time::Instant) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        let idle = self.governor.listings_every(self.config.host.poll_interval);
        let (due, shortest) = self.repositories.iter_listed().fold(
            (None::<tokio::time::Instant>, idle),
            |(due, shortest), (name, state)| {
                let every = self.listing_every(name);
                let at = state.pace().due(every).unwrap_or(now);
                (Some(due.map_or(at, |due| due.min(at))), shortest.min(every))
            },
        );
        due.unwrap_or(anchor + idle)
            .max(now + shortest.min(TICK_REST))
    }

    /// Notes that a runner of `repository` started or went: its next probe lists its runners.
    pub(super) fn owe_runners_listing(&mut self, repository: &str) {
        if let Some(state) = self.repositories.listed_mut(repository) {
            state.pace_mut().runners_owed();
        }
    }

    /// Each configured repository in `due` with the labels of its pools, whose queued jobs a tick lists.
    fn to_list(&self, due: &BTreeSet<String>) -> Vec<ToList> {
        self.config
            .repository_names()
            .into_iter()
            .filter(|repository| due.contains(repository))
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
    /// None starts once a stop is asked for, even mid-plan. Commits the plan's slot waits first, as each plan a tick
    /// carries out has its own (ADR 0110).
    async fn apply(
        &mut self,
        plan: Plan,
        admitting: &mut Admitting,
        mode: Shutdown,
        pace: &mut Pace,
    ) -> Applied {
        self.note_slots(plan.slot_waiting);
        let mut applied = Applied {
            held: plan.held,
            barrier: plan.barrier,
            ..Applied::default()
        };
        let (stops, decisions): (Vec<Decision>, Vec<Decision>) = plan
            .decisions
            .into_iter()
            .partition(|decision| matches!(decision, Decision::Stop { .. }));
        let stops: Vec<(SlotRef, StopReason)> = stops
            .into_iter()
            .filter_map(|decision| match decision {
                Decision::Stop { slot, reason } => Some((slot, reason)),
                Decision::Start { .. } => None,
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
        pace.lap(Phase::Removing, self.passes_in_tick);
        for carried in self.start_batch(decisions, admitting).await {
            match carried {
                Carried::Done(StartOutcome::Started) => {
                    applied.acted = true;
                    pace.started += 1;
                }
                // It changed nothing a plan again would start more by.
                Carried::Done(StartOutcome::Failed) | Carried::Skipped => {}
                Carried::Deferred => applied.deferred += 1,
            }
        }
        pace.lap(Phase::Starting, self.passes_in_tick);
        applied
    }

    /// What a stop in `mode` removes: every runner when forced, the idle ones when graceful, none when it detaches,
    /// which leaves them to the next start.
    fn stops(&self, mode: Shutdown) -> Plan {
        Plan {
            decisions: self
                .occupied()
                .filter(|(_, occupant)| {
                    mode == Shutdown::Force || mode == Shutdown::Graceful && !occupant.listed_busy()
                })
                .map(|(slot, _)| Decision::Stop {
                    slot,
                    reason: StopReason::Shutdown,
                })
                .collect(),
            ..Plan::default()
        }
    }

    /// Plans from the last poll's listings, as they stand now.
    pub(super) fn plan(&self) -> (Plan, Admitting) {
        let taken = self.taken();
        let warm = self.warm_targets();
        let mut repositories: Vec<Repository> = self
            .config
            .repositories
            .iter()
            .filter_map(|repository| {
                // Every repository of the config is listed: a reload lists them with the config it takes.
                let state = self.repositories.listed(&repository.name)?;
                Some(Repository {
                    name: repository.name.clone(),
                    profile: repository.profile.clone(),
                    labels: repository.labels.clone(),
                    paused: self.control.is_paused(&repository.name),
                    held: state.health(&repository.profile).and_then(PoolHealth::held),
                    queued: self.listed_demand(repository, &taken),
                    runner_memory: None,
                    runner_cpu: None,
                    waits: state
                        .waits(&repository.profile)
                        .copied()
                        .unwrap_or_default(),
                    usual: None,
                    warm: warm.get(&Pool::from(repository)).copied().unwrap_or(0),
                })
            })
            .collect();
        self.usual_runs(&mut repositories, &taken);
        let profiles = self
            .config
            .profiles
            .iter()
            .map(|(name, profile)| (name.clone(), self.plan_profile(name, profile)))
            .collect();
        let free_memory = self
            .memory_max
            .map(|max| max.saturating_sub(self.count(self.host_remembered()).pooled()));
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
                    occupant.idle()
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

    /// Notes that a runner of `repository`'s pool in `profile` may have taken one of its listed jobs without the
    /// engine knowing which (`Claims::unnamed`).
    pub(super) fn spend(&mut self, repository: &str, profile: &str) {
        self.claims.spend(repository, profile, self.listing);
    }

    /// Notes that a runner of this machine took `job` of `repository` and is gone (`Claims::consumed`).
    pub(super) fn consume(&mut self, repository: &str, job: u64) {
        self.claims.consume(repository, job, self.listing.listing);
    }

    /// The jobs `pool`'s repository's last listing showed queued that may still want a runner (ADR 0018, 0095).
    /// GitHub lists a job queued for a while after a runner took it, and a listing may have begun before that, so
    /// not those its captures named (`taken`), nor those of runners gone since (`consumed`); and one fewer, the oldest
    /// the pool matches, for each runner of the pool that may have taken one no capture named: one a listing first
    /// showed busy, and one gone since the listing began (`unnamed`). Until the job is named, a pool may get one
    /// runner too few rather than two for one job.
    pub(super) fn listed_demand(
        &self,
        pool: &crate::config::Repository,
        taken: &BTreeSet<u64>,
    ) -> Vec<QueuedJob> {
        let busy = self
            .slots
            .get(&pool.profile)
            .into_iter()
            .flatten()
            .filter_map(super::Slot::occupant)
            .filter(|occupant| occupant.repository == pool.name && occupant.claims_a_listed_job())
            .count();
        let gone = self.claims.unnamed(&pool.name, &pool.profile);
        let unnamed = busy + gone;
        let mut queued = self.listed_unnamed(&pool.name, taken);
        let claimed: BTreeSet<u64> = {
            let mut matching: Vec<u64> = queued
                .iter()
                .filter(|job| crate::scheduler::matches(job, &pool.labels))
                .map(|job| job.id)
                .collect();
            matching.sort_unstable();
            matching.into_iter().take(unnamed).collect()
        };
        queued.retain(|job| !claimed.contains(&job.id));
        queued
    }

    /// The jobs listed on `repository` that no runner of this machine is known to have taken: not in `taken`, nor
    /// consumed by a claim.
    pub(super) fn listed_unnamed(&self, repository: &str, taken: &BTreeSet<u64>) -> Vec<QueuedJob> {
        self.repositories
            .listed(repository)
            .map(|state| state.listing().jobs())
            .unwrap_or_default()
            .iter()
            .filter(|job| !taken.contains(&job.id) && !self.claims.consumed(repository, job.id))
            .cloned()
            .collect()
    }

    /// The budgets a plan has beside what runs now, as `admitting` admits. `drained` keeps what no runner's end gives
    /// back and reads none of the interlocks' zeros. For a job the host holds, the host's limit in it is the larger
    /// margin's while a runner runs, whichever margin is in force: that one follows the jobs waiting (ADR 0102), and a
    /// barrier that followed it would take them from the waiters that decide it. The empty slice's plain margin counts
    /// only once it is empty, as a warm runner may keep it from ever being. For any other job it is the plain margin's.
    pub(super) fn plan_budget(&self, admitting: &Admitting) -> Budget {
        let committed = self.committed_memory();
        let count = self.count(admitting.remembered);
        let running = self.footprints().next().is_some();
        Budget {
            // Signed, as the host's: a warm runner's room pays back what runs past the budget first.
            memory: if admitting.held {
                Some(0)
            } else {
                self.memory_max
                    .map(|max| crate::scheduler::left(max, count.pooled()))
            },
            cpu: admitting
                .cpu
                .as_ref()
                .and(self.cpu_budget())
                .map(|budget| crate::scheduler::left(budget, self.cpu_in_use())),
            host: admitting
                .host
                .as_ref()
                .map(|host| host::left(host.limit, count.hosted(), host.stalled)),
            held: admitting.held || admitting.host.as_ref().is_some_and(|host| host.stalled),
            credit_room: self.most_credit().saturating_sub(count.credit),
            host_credit_room: count.host_credit_room,
            drained: Drained {
                memory: self
                    .memory_max
                    .map(|max| crate::scheduler::left(max, committed)),
                cpu: admitting
                    .cpu
                    .as_ref()
                    .and(self.cpu_budget())
                    .map(|budget| crate::scheduler::left(budget, 0)),
                host: admitting.host.as_ref().map(|host| {
                    let limit = host::limit(&host.memory, running || host.stalled);
                    crate::scheduler::left(limit, committed)
                }),
                host_plain: admitting.host.as_ref().map(|host| {
                    crate::scheduler::left(host::limit(&host.memory, false), committed)
                }),
            },
        }
    }

    /// Whether a runner of `memory` and `cpu` that `plan` started in `profile` still fits, by the plan's rule, the
    /// budgets as they are now: the samples and host readings taken since, and what ran past what the plan counted
    /// on, such as a runner whose stop failed. Without a recent sample of the slice, measured admission counts it at
    /// its cap, as the plan would then. The host keeps the margin the plan chose: the runners a plan starts into an
    /// empty slice all need only the first runner's.
    pub(super) fn still_fits(
        &self,
        plan: &Admitting,
        profile: &str,
        memory: u64,
        cpu: u64,
    ) -> bool {
        let mut now = self.admitting(&mut [], None);
        now.host = self.host_budget_by(plan.remembered);
        now.remembered = plan.remembered;
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
            .saturating_sub(slots.iter().filter(|slot| !slot.is_free()).count());
        let slots_len = slots.len();
        let slots = slots
            .iter()
            .map(|slot| match slot.holder() {
                Holder::Free if free > 0 => {
                    free -= 1;
                    Slot::Free
                }
                Holder::Free => Slot::Closed,
                // A start in flight is a runner its plan gave a job or a warm purpose already: none to start again.
                Holder::Starting(starting) => {
                    let footprint = starting.footprint();
                    Slot::Runner(RunnerSlot {
                        repository: starting.repository().to_owned(),
                        busy: true,
                        started_at: starting.launched_at(),
                        memory: self.held_by(&footprint).takes,
                        cpu: self.reserves_cpu(&footprint),
                        usual: None,
                        behind: starting.behind(),
                    })
                }
                Holder::Runner(occupant) => Slot::Runner(RunnerSlot {
                    repository: occupant.repository.clone(),
                    // Its capture named its job before a listing showed it busy: it waits for none of those queued.
                    busy: occupant.engaged(),
                    started_at: occupant.started_at,
                    memory: self.takes(occupant),
                    cpu: self.reserves_cpu(&occupant.footprint()),
                    usual: self.usual_run(occupant, name),
                    behind: occupant.engagement.behind(),
                }),
            })
            .collect();
        Profile {
            count: profile.slots as usize,
            memory: profile.memory.0,
            cpus: (u64::from(profile.cpus) * 1000).min(self.cpu_budget().unwrap_or(u64::MAX)),
            upkeep_due: self.upkeep_waits_for_warm(name),
            preparing: self.preparing(name, profile),
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

    /// What keeps every slot of the profile from starting for now.
    pub(super) fn preparing(
        &self,
        name: &str,
        profile: &crate::config::Profile,
    ) -> Option<Preparing> {
        if self.is_reseeding(name) {
            Some(Preparing::Reseeding)
        } else if self.awaits_image(&profile.image) {
            Some(Preparing::PullingImage)
        } else {
            None
        }
    }

    /// Whether a slot holds a runner.
    pub(super) fn holds_runner(&self, slot: &SlotRef) -> bool {
        self.slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .is_some_and(|slot| !slot.is_free())
    }

    /// The part of the plain count no runner's end gives back: the seed containers and the room waiting ones hold, the
    /// shells and the mirror.
    pub(super) fn committed_memory(&self) -> u64 {
        let reseeds: u64 = self.seed_memory().values().sum();
        reseeds + self.shell_memory() + self.mirror_memory_in_use()
    }

    /// What a runner takes of the budget.
    pub(super) fn takes(&self, occupant: &Occupant) -> u64 {
        self.held_by(&occupant.footprint()).takes
    }

    /// What a runner, or a start in flight as the runner it becomes, takes of the budget and what it is known to use.
    fn held_by(&self, occupant: &Footprint<'_>) -> Held {
        match self.config.host.admission {
            // Whatever it reserved under measured admission before a reload.
            Admission::Caps => Held {
                takes: occupant.memory,
                used: Reading::Stale,
            },
            // Nothing once its container's cgroup is gone: its job ended and its memory with it, though its slot waits
            // for its log and its removal, for 20 s after a burst.
            Admission::Measured if self.store.container_gone(occupant.runner_name) => Held {
                takes: 0,
                used: Reading::Stale,
            },
            // What it uses when that is more: a job past its prediction, or one in a burst of starts. Its cap when
            // that is not known: the slice's or its own last sample is older than three intervals, as when the
            // sampler stalls or cannot read its container.
            Admission::Measured => {
                let now = self.clock.now_ms();
                let fresh = 3 * u64::try_from(SAMPLE_INTERVAL.as_millis()).unwrap_or(u64::MAX);
                let slice_sampled = self
                    .store
                    .latest_host_sample()
                    .is_some_and(|sample| sample.at.saturating_mul(1000) + fresh >= now);
                let own = self.store.latest_sample(occupant.runner_name);
                // Before its first sample, a runner is as fresh as its start.
                let sampled_at = own
                    .as_ref()
                    .map_or_else(|| time::ms(occupant.started_at), |sample| sample.at);
                if !slice_sampled || sampled_at + fresh < now {
                    Held {
                        takes: occupant.memory,
                        used: Reading::Stale,
                    }
                } else {
                    let used = own.map(|sample| sample.memory);
                    Held {
                        takes: occupant.reserved.max(used.unwrap_or(0)),
                        used: used.map_or(Reading::NotYet, Reading::Used),
                    }
                }
            }
        }
    }

    /// The most the pooled count may take off the plain one: none under caps or with `[host] pooled_count` off.
    fn most_credit(&self) -> u64 {
        let measured = self.config.host.admission == Admission::Measured;
        if measured && self.config.host.pooled_count {
            pooled::MOST_CREDIT
        } else {
            0
        }
    }

    /// Whether the host's limit may take its credit (ADR 0108): under measured admission with `[host] pooled_host`.
    fn host_reserve(&self) -> bool {
        self.config.host.admission == Admission::Measured && self.config.host.pooled_host
    }

    /// The aggregate count of what runs (ADR 0099, 0108), the host going by the margin `remembered` asks for.
    pub(super) fn count(&self, remembered: host::Remembered) -> pooled::Count {
        let now = SystemTime::now();
        let runners: Vec<pooled::Runner<'_>> = self
            .footprints()
            .map(|footprint| {
                let held = self.held_by(&footprint);
                pooled::Runner {
                    key: footprint.key,
                    takes: held.takes,
                    used: held.used,
                    // An adopted runner's job may have started under another daemon.
                    ran: footprint
                        .job_found_here
                        .and_then(|started| now.duration_since(started).ok()),
                    basis: footprint.basis,
                }
            })
            .collect();
        let fresh = (3 * SAMPLE_INTERVAL).as_secs();
        let slice = self
            .store
            .latest_host_sample()
            .filter(|sample| sample.at + fresh >= self.clock.now_ms() / 1000)
            .map(|sample| pooled::Slice {
                memory: sample.memory,
                memory_max: self.memory_max,
            });
        let host = self
            .host_memory()
            .map(|memory| host::pooling(&memory, remembered));
        pooled::count(&pooled::Snapshot {
            runners: &runners,
            committed: self.committed_memory(),
            slice,
            host,
            most: self.most_credit(),
            host_reserve: self.host_reserve(),
            largest: None,
        })
    }

    /// The mirror's part of `memory_in_use`.
    pub(super) fn mirror_memory_in_use(&self) -> u64 {
        // The configured mirror is reserved for; a running one keeps the cap it started with until it stops.
        let mirror = &self.config.caches.mirror;
        let configured = if mirror.enabled { mirror.memory.0 } else { 0 };
        configured.max(self.mirror.running_memory())
    }

    /// Takes each repository's queued jobs from its listing, begun at `listing`. A repository a command removed
    /// meanwhile is skipped, but the jobs its listing recorded are followed until GitHub says what became of them. A
    /// claim on a listed job made after the listing began, or less than `poll_interval` before, outlives it
    /// (`Mark::ends`).
    fn apply_queued(&mut self, listings: Vec<Listed>, listing: Mark) {
        let floor = self.config.host.poll_interval;
        let taken = self.taken();
        for Listed {
            repository,
            answer,
            recorded,
        } in listings
        {
            let mut listed = false;
            let queued = match answer {
                Ok(queued) => {
                    listed = true;
                    queued
                }
                Err(github::Error::RateLimited { until }) => {
                    self.github_limited(until);
                    Vec::new()
                }
                Err(error) => {
                    warn!("{repository}: cannot list queued jobs: {error}");
                    Vec::new()
                }
            };
            if listed {
                self.claims
                    .listing_answered(&repository, listing, floor, &queued);
                for occupant in self
                    .slots
                    .values_mut()
                    .flatten()
                    .filter_map(super::Slot::occupant_mut)
                    .filter(|occupant| occupant.repository == repository)
                {
                    occupant.engagement.queued_listed(listing, floor);
                }
            }
            if let Some(state) = self.repositories.listed_mut(&repository) {
                // Before the plan: a start for the next job must not read the note of one that left the queue.
                if listed {
                    for waits in state.all_waits_mut() {
                        waits.retain(|job| queued.iter().any(|listed| listed.id == job));
                    }
                    state.listing_mut().answer(queued);
                } else {
                    state.listing_mut().fail();
                }
            } else {
                for job in recorded.into_iter().filter(|job| !taken.contains(job)) {
                    self.ledger.follow_queued(job, &repository);
                }
            }
        }
    }

    /// Updates each runner's busy flag from the listings, begun at `listing`, and deletes this host's runners that no
    /// slot holds. A repository whose runners listing failed has them listed at its next probe.
    async fn apply_runners(&mut self, listings: Answers<Vec<Runner>>, listing: Mark) {
        for (repository, answer) in listings {
            if answer.is_err() {
                self.owe_runners_listing(&repository);
            }
            // Whether this machine still has a runner registered on it.
            let mut left = false;
            let runners = match answer {
                Ok(runners) => runners,
                Err(github::Error::RateLimited { until }) => {
                    self.github_limited(until);
                    // Not listed: a drain's failure clock starts over.
                    self.repositories
                        .runners_listed(&repository, RunnersListing::RateLimited);
                    continue;
                }
                Err(error) => {
                    self.drain_failed(&repository, &error);
                    continue;
                }
            };
            let listed: BTreeSet<u64> = runners.iter().map(|runner| runner.id).collect();
            for runner in runners {
                let held = self
                    .slots
                    .values_mut()
                    .flatten()
                    .filter_map(super::Slot::occupant_mut)
                    .find(|occupant| occupant.runner_id == runner.id);
                match held {
                    Some(occupant) => {
                        occupant.engagement.listed(runner.busy, listing);
                        left = true;
                    }
                    // Only this machine's names: other machines serve the same repositories.
                    None if !runner.name.starts_with(&self.runner_prefix) => {}
                    None => match self.github.delete_runner(&repository, runner.id).await {
                        // One released here whose registration GitHub kept past its job: nothing went wrong.
                        Ok(Deleted::Deleted) if self.registrations.is_ended_here(runner.id) => {}
                        Ok(Deleted::Deleted) => {
                            info!(
                                "{repository}: deleted {}, which had no container",
                                runner.name
                            );
                        }
                        Ok(Deleted::NotFound) => {}
                        Ok(Deleted::Busy) => {
                            left = true;
                            if self.registrations.refused(runner.id, &repository) {
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
            self.registrations.listed(&repository, &listed);
            self.repositories
                .runners_listed(&repository, RunnersListing::Answered { left });
        }
    }

    /// Forgets what only a listing of a repository could end, once neither the config nor a drain lists it: a drain
    /// that gave up, or one that ended, leaves no listing to do it.
    fn forget_unlisted(&mut self) {
        let repositories = &self.repositories;
        let listed = |name: &str| repositories.is_known(name);
        self.registrations.forget_unlisted(listed);
        self.claims.forget_unlisted(listed);
    }

    /// Warns of a runners listing that failed, and stops draining its repository once GitHub says it is gone, as when
    /// the token lost it, or after `DRAIN_FAILING_FOR` of failed listings in a row: this machine's runners still
    /// registered there stay. A 403 waits for the clock, as a secondary rate limit can answer one.
    fn drain_failed(&mut self, repository: &str, error: &github::Error) {
        let failed = RunnersListing::Failed {
            gone: error.is_gone(),
        };
        if self.repositories.runners_listed(repository, failed) {
            warn!(
                "{repository}: stops draining, so any of this machine's runners still registered there stay: \
                 cannot list its runners: {error}"
            );
        } else {
            warn!("{repository}: cannot list runners: {error}");
        }
    }
}
