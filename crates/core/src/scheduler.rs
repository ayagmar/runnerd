//! Which runners to start and stop, as a pure function of queued jobs, slots, warm runners and the memory and CPU
//! budgets (`plan`), how many warm runners each repository wants (`warm`), and the loop that applies those decisions
//! with GitHub and Docker (`engine`). `plan` and `warm` do no I/O and read no clock.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use crate::admission::Key;
use crate::api::Waiting;
use crate::github::QueuedJob;

pub mod engine;
pub mod priority;
mod waits;
pub mod warm;

use priority::{Around, Backfill, Candidate, Caps, Held, Older, Runner};
pub use priority::{Barrier, Drained, PRIORITY_AFTER, SERVED_PRIORITY_AFTER, Scarce};
pub use waits::{Wait, Waits};

/// How long a started runner may wait for a job before it counts as surplus: covers the container's start and
/// GitHub's assignment.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// A repository's runners held back after they failed.
#[derive(Debug, Clone, Copy)]
pub struct Hold {
    /// No runner starts for it before then.
    pub until: SystemTime,
    /// What failed.
    pub cause: crate::api::HoldCause,
}

/// Names one of a repository's pools: its runners in one profile (ADR 0058). No two pools of a repository share a
/// profile.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pool {
    /// `owner/name` on GitHub.
    pub repository: String,
    /// The profile its runners start in.
    pub profile: String,
}

impl From<&crate::config::Repository> for Pool {
    fn from(repository: &crate::config::Repository) -> Pool {
        Pool {
            repository: repository.name.clone(),
            profile: repository.profile.clone(),
        }
    }
}

/// One of a repository's pools as the planner sees it: its runners in one profile, registered with one set of labels
/// (ADR 0058). A repository with one `[[repositories]]` entry has one.
#[derive(Debug, Clone)]
pub struct Repository {
    /// `owner/name` on GitHub.
    pub name: String,
    /// The profile the jobs its labels match run in.
    pub profile: String,
    /// The labels its runners register with.
    pub labels: Vec<String>,
    /// Paused repositories get no new runners; their running jobs finish.
    pub paused: bool,
    /// No runner starts for this repository before then, after its runners failed.
    pub held: Option<Hold>,
    /// The repository's queued jobs at the last poll, those of its other pools too.
    pub queued: Vec<QueuedJob>,
    /// What a new runner of it takes of the budget: `None`, its profile's `memory`. Under measured admission, the
    /// largest predicted peak of the queued jobs its labels match, since GitHub may give the runner any of them.
    pub runner_memory: Option<u64>,
    /// What a new runner of it takes of the CPU budget, in millicores: `None`, its profile's `cpus`. Under measured
    /// admission, the largest predicted average CPU of its queued jobs.
    pub runner_cpu: Option<u64>,
    /// What its next job has waited for, and since when (ADR 0096), and whether a barrier has served it (ADR 0098).
    pub waits: Waits,
    /// The largest usual run time among the queued jobs its labels match, since GitHub may give a runner any of them;
    /// `None` if any has no estimate, and then none of its jobs starts behind a barrier (ADR 0097).
    pub usual: Option<Duration>,
    /// How many idle runners it keeps beyond its queued jobs, for jobs not queued yet (`warm::target`).
    pub warm: usize,
}

impl Repository {
    /// Its name as a pool.
    pub fn pool(&self) -> Pool {
        Pool {
            repository: self.name.clone(),
            profile: self.profile.clone(),
        }
    }
}

/// A profile's slots: one concurrent job each.
#[derive(Debug, Clone)]
pub struct Profile {
    /// Memory a slot's container may use.
    pub memory: u64,
    /// CPU a slot's container may use, in millicores; under a CPU budget, at most the budget, as a runner at its caps
    /// reserves it.
    pub cpus: u64,
    /// While the profile's snapshot is rebuilt or its image pulled, none of its slots starts.
    pub preparing: Option<Preparing>,
    /// Upkeep waits for its runners, all idle: its reseed (slot 1 ran a job since the last), or the shared caches'
    /// check, which waits for every profile's. Its warm runners stop, and none starts, so the upkeep can run.
    pub upkeep_due: bool,
    /// Its configured `slots`. A reload that lowered them leaves runners past them, whose slots close once they end.
    pub count: usize,
    /// Index 0 is slot 1.
    pub slots: Vec<Slot>,
    /// The job each slot ran last, which left its caches in the slot's layer (ADR 0080), by slot index; `None`, or
    /// no entry, where none is known.
    pub ran: Vec<Option<Key>>,
}

/// Why none of a profile's slots starts for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preparing {
    /// Slot 1's layers become the profile's snapshot.
    Reseeding,
    /// Its image is being pulled.
    PullingImage,
}

/// What a slot holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// Nothing: a runner may start here.
    Free,
    /// A runner, waiting for its job or running it.
    Runner(RunnerSlot),
    /// Free, but over the profile's number of slots, which a reload lowered while its runners ran: nothing starts
    /// here.
    Closed,
}

/// A slot whose container runs a runner registered for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSlot {
    /// `owner/name` the runner is registered on.
    pub repository: String,
    /// Whether GitHub reports the runner running a job.
    pub busy: bool,
    /// When its container started.
    pub started_at: SystemTime,
    /// What it takes of the memory budget, which it gives back if it stops.
    pub memory: u64,
    /// What it takes of the CPU budget, in millicores.
    pub cpu: u64,
    /// How long its job usually keeps it from `started_at`, once that job and its runs are known (ADR 0097).
    pub usual: Option<Duration>,
    /// The owner of the barrier it started behind (ADR 0131); `None` for one started otherwise, warm, or adopted by a
    /// restart, which loses it with the waits it relates to.
    pub behind: Option<u64>,
}

/// Where slots are, by profile name and slot index (0 is slot 1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotRef {
    /// The profile's name.
    pub profile: String,
    /// 0 is slot 1.
    pub index: usize,
}

/// What `plan` decides for one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Register a runner for the repository and start it in the slot.
    Start {
        /// The free slot.
        slot: SlotRef,
        /// `owner/name` the runner registers on.
        repository: String,
        /// What it takes of the budget.
        memory: u64,
        /// What it takes of the CPU budget, in millicores.
        cpu: u64,
        /// Whom it starts for: a queued job, or none as a warm runner.
        for_: StartFor,
        /// The warm runners this plan stops to make room for it, just before: if one is still there, it took a job
        /// meanwhile and the start waits for the next plan.
        replacing: Vec<SlotRef>,
        /// The barrier's owner, for a start behind a barrier (ADR 0097).
        behind: Option<u64>,
    },
    /// Remove the slot's idle runner, for `reason`.
    Stop {
        /// The slot whose runner stops.
        slot: SlotRef,
        /// Why it stops.
        reason: StopReason,
    },
}

/// Whom a plan starts a runner for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFor {
    /// The queued job with this ID: its wait ends once the runner is in its slot (ADR 0110).
    Job(u64),
    /// No queued job: a warm runner, at its profile's caps.
    Warm,
}

/// A pool's next job that lacks something scarce, as the plan found it: past the jobs its idle runners take, and
/// those the plan starts a runner for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demand {
    /// The pool.
    pub pool: Pool,
    /// The job's ID.
    pub job: u64,
    /// What it lacks.
    pub scarce: Scarce,
}

/// Why an idle runner is removed: what its removal log line and the history say (ADR 0092).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// It waited `IDLE_TIMEOUT`, and neither a queued job nor its repository's warm runners want it.
    Expired,
    /// A queued job of another pool takes its room.
    Room,
    /// Its profile's reseed or the shared caches' check waits for its slot.
    Upkeep,
    /// Its repository is paused.
    Paused,
    /// Its slot is past a lowered slot count.
    Closed,
    /// A reload changed its pool or profile, or a newer image or container spec replaces it.
    Changed,
    /// It was raised for a heavier queued job and no longer fits the budget.
    OverBudget,
    /// A cache clean needs the volumes it mounts.
    Clean,
    /// runnerd stops.
    Shutdown,
}

impl StopReason {
    /// How the history stores it.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Expired => "expired",
            StopReason::Room => "room",
            StopReason::Upkeep => "upkeep",
            StopReason::Paused => "paused",
            StopReason::Closed => "closed",
            StopReason::Changed => "changed",
            StopReason::OverBudget => "over_budget",
            StopReason::Clean => "clean",
            StopReason::Shutdown => "shutdown",
        }
    }

    /// Why, as the removal's log line says it.
    pub fn words(self) -> &'static str {
        match self {
            StopReason::Expired => "no job came for it",
            StopReason::Room => "a queued job takes its room",
            StopReason::Upkeep => "its profile's upkeep waits for it",
            StopReason::Paused => "its repository is paused",
            StopReason::Closed => "its slot closed",
            StopReason::Changed => "the config or runnerd changed its runners",
            StopReason::OverBudget => "raised for a heavier job, it does not fit the budget",
            StopReason::Clean => "a cache clean needs its volumes",
            StopReason::Shutdown => "runnerd stops",
        }
    }
}

/// What `plan` decides, and why each queued job it starts no runner for waits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// What to start and stop.
    pub decisions: Vec<Decision>,
    /// Why each queued job of a repository's matching jobs waits, by job ID. A job an idle runner will take is
    /// `Waiting::RunnerStarting`; a job this plan starts a runner for has no reason, as that runner does not exist
    /// until the start succeeds.
    pub waiting: BTreeMap<u64, Waiting>,
    /// The idle runners kept warm: no queued job of their repository waits for them.
    pub warm: BTreeSet<SlotRef>,
    /// The queued jobs one more slot of their profile would start now: none of its slots is free or a warm runner's
    /// to give, while the memory and CPU budgets, less what the older of these jobs would take, have room for their
    /// runners. Whatever `waiting` calls it, `NoSlot` or `BehindStart`; a job that also lacks memory or CPU is not
    /// one (ADR 0090), nor is one a barrier holds, which another slot would not start.
    pub slot_waiting: BTreeSet<u64>,
    /// The pools' next jobs a slot had room for and memory or CPU not: one per pool and resource. What a wait starts
    /// from, once a scheduling path commits the plan (ADR 0096).
    pub held: Vec<Demand>,
    /// The job no younger job starts before, if one waited long enough.
    pub barrier: Option<Barrier>,
}

/// The budgets a plan starts runners within.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budget {
    /// The memory left, negative by what the running containers take past `MemoryMax` (`left`); `None` for no
    /// budget.
    pub memory: Option<i64>,
    /// The CPU left, in millicores, negative by what the runners reserve past the budget (`left`); `None` for no
    /// budget.
    pub cpu: Option<i64>,
    /// The memory the host has to spare for new runners beside other programs (ADR 0055), negative by what the
    /// running ones reserve past the host's limit; `None` when not known.
    pub host: Option<i64>,
    /// Nothing starts, so no warm runner stops to make room: measured admission holds the slice at its high-water
    /// mark, or the host's tasks stall for memory. `memory` and `host` are then 0 whatever the room, so no job's wait
    /// starts and none puts up a barrier. Not a reservation for a starved job (ADR 0096).
    pub held: bool,
    /// What the budgets would leave with no runner: what a job may hold younger ones for.
    pub drained: Drained,
    /// How much more the pooled count could take off what runs with no runner ending (ADR 0099): room that may come at
    /// any moment, which a backfill must not outlast. 0 with pooling off.
    pub credit_room: u64,
    /// How much more the host's credit could take off what runs with no runner ending (ADR 0108), as `credit_room`
    /// does for `MemoryMax`'s. 0 with `[host] pooled_host` off.
    pub host_credit_room: u64,
}

/// The workflow labels of GitHub-hosted runners, from
/// <https://github.com/github/docs/blob/main/data/reusables/actions/supported-github-runners.md> and its single-CPU
/// row (`single-cpu-table-row.md`), and the larger macOS runners of
/// <https://github.com/github/docs/blob/main/data/reusables/actions/larger-runners-table.md>, as of 2026-10-16. Update it when GitHub adds or retires an image: a label
/// missing here makes its jobs wait with `Waiting::NoPool`, whose wording stays true for them.
const GITHUB_HOSTED: &[&str] = &[
    "ubuntu-slim",
    "ubuntu-latest",
    "ubuntu-26.04",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "ubuntu-26.04-arm",
    "ubuntu-24.04-arm",
    "ubuntu-22.04-arm",
    "windows-latest",
    "windows-2025",
    "windows-2025-vs2026",
    "windows-2022",
    "windows-11-arm",
    "windows-11-vs2026-arm",
    "macos-latest",
    "macos-26",
    "macos-15",
    "macos-14",
    "macos-26-intel",
    "macos-15-intel",
    "xcode-27",
    "macos-latest-large",
    "macos-26-large",
    "macos-15-large",
    "macos-14-large",
    "macos-latest-xlarge",
    "macos-26-xlarge",
    "macos-15-xlarge",
    "macos-14-xlarge",
    "xcode-27-xlarge",
];

/// Whether GitHub runs a job on its own runners: it asks for exactly one label, one of `GITHUB_HOSTED`. No pool of
/// this daemon is meant for it. A typo, a custom label and an image name beside `self-hosted` are the owner's to
/// fix, so they wait for a pool instead.
fn hosted_by_github(job: &QueuedJob) -> bool {
    matches!(&job.labels[..], [label] if GITHUB_HOSTED.iter().any(|hosted| hosted.eq_ignore_ascii_case(label)))
}

/// Whether a job may run on a repository's runners: every label it asks for is one of theirs. GitHub compares
/// labels without regard to case.
pub fn matches(job: &QueuedJob, labels: &[String]) -> bool {
    crate::config::labels_match(&job.labels, labels)
}

/// Decides which slots start a runner and which idle runners stop, and why each job left queued waits.
///
/// `repositories` holds each repository's pools; a job is served by the pool whose labels match it, and one no pool
/// of its repository matches waits for a pool (`Waiting::NoPool`) unless it is for GitHub's own runners. Each pool
/// gets a runner per matching queued job, less its runners still waiting for a job. Jobs are
/// served oldest first across repositories, each in a free slot of its repository's profile (`free_slot`: the one
/// that ran the same job last, else one that ran its repository, else the lowest), while what the runner takes (`Repository::runner_memory`, by default the slot's memory) fits `budget.memory` and
/// `budget.host`, and its CPU
/// (`Repository::runner_cpu`, by default the slot's `cpus`) fits `budget.cpu` (`None`: no budget). A job that does
/// not fit takes the room of warm runners of other repositories, which stop. A pool whose job had a slot but not
/// memory or CPU is held once for each (`Plan::held`). An idle runner stops once no queued job is left for it,
/// it is not one of its repository's `warm` runners, and it waited `IDLE_TIMEOUT`. The first such job that waited
/// long enough, and that draining the running jobs would make room for, puts up the plan's one barrier
/// (`priority::Barrier`): no younger job starts until a runner of its pool does, unless it fits and `Backfill` says it
/// costs the owner neither of ADR 0097's bounds. Last, while no job waits for room, each repository short of its `warm`
/// runners starts them at its profile's caps, one per repository at a time.
pub fn plan(
    repositories: &[Repository],
    profiles: &BTreeMap<String, Profile>,
    budget: Budget,
    now: SystemTime,
) -> Plan {
    let mut decided = Plan::default();
    unserved(repositories, &mut decided);
    let ToServe {
        mut wanted,
        jobs,
        warm: warm_wanted,
    } = to_serve(repositories, profiles, now, &mut decided);

    let mut warm = Warm::new(&decided.warm, profiles, budget.held);
    let mut taken = taken_slots(profiles);
    let mut left = Left {
        memory: budget.memory,
        cpu: budget.cpu,
        host: budget.host,
    };
    let mut started = Started::new(&taken, left);
    let mut slot_bound = SlotBound::default();
    // Under a safety interlock, what is left says nothing of the room there is.
    let drained = (!budget.held).then_some(budget.drained);
    let mut behind = Behind::new(profiles);
    for (position, &(job, repository)) in jobs.iter().enumerate() {
        let Some(missing) = wanted.get_mut(&pool_key(repository)) else {
            continue;
        };
        if *missing == 0 {
            continue;
        }
        let (Some(profile), Some(taken)) = (
            profiles.get(&repository.profile),
            taken.get_mut(repository.profile.as_str()),
        ) else {
            continue;
        };
        let memory = repository.runner_memory.unwrap_or(profile.memory);
        let cpu = repository.runner_cpu.unwrap_or(profile.cpus);
        let free_index = free_slot(taken, profile, repository, job, &jobs[position + 1..]);
        let fits = free_index.is_some() && left.fits(memory, cpu);
        let turn = Turn {
            repository,
            profile,
            job,
            memory,
            cpu,
            free_slot: free_index.is_some(),
            left,
            started: &started,
        };
        if let Some(barrier) = decided.barrier {
            let free_slots = taken.iter().filter(|taken| !**taken).count();
            if !fits || !behind.admits(repository, profile, free_slots) {
                turn.behind(barrier, &mut decided);
                continue;
            }
        }
        let room = if fits || profile.preparing.is_some() {
            None
        } else {
            warm.make_room(repository, free_index.is_none(), left, memory, cpu)
        };
        let index = match (room.as_ref(), free_index) {
            (Some(room), free) => free.or(room.slot).unwrap_or_default(),
            (None, Some(index)) if fits => index,
            (None, _) => {
                turn.waits(drained, &mut slot_bound, now, &mut decided);
                let job = Older {
                    profile: &repository.profile,
                    free_slot: free_index.is_some() || profile.preparing.is_some(),
                    memory,
                    cpu,
                };
                behind.left_waiting(job, decided.barrier, left, &budget, now);
                continue;
            }
        };
        let replacing = room.map_or_else(Vec::new, |room| stop_for(room, &mut left, &mut decided));
        taken[index] = true;
        *missing -= 1;
        started.note(&repository.profile, job);
        left.take(memory, cpu);
        behind.started(
            &repository.profile,
            profile.caps(),
            memory,
            cpu,
            decided.barrier,
            now,
        );
        decided.decisions.push(start(
            repository,
            job,
            index,
            memory,
            cpu,
            replacing,
            decided.barrier,
        ));
    }
    if !budget.held {
        start_warm(&warm_wanted, profiles, &mut taken, left, &mut decided);
    }
    decided
}

/// Stops the warm runners whose `room` a start takes, before it: they give back what they took.
fn stop_for(room: Room, left: &mut Left, decided: &mut Plan) -> Vec<SlotRef> {
    left.give(room.memory, room.cpu);
    for slot in &room.stopped {
        decided.warm.remove(slot);
        decided.decisions.push(Decision::Stop {
            slot: slot.clone(),
            reason: StopReason::Room,
        });
    }
    room.stopped
}

/// What a plan gathers for what may start behind its barrier.
struct Behind<'plan> {
    /// The runners in slots, and those started so far.
    runners: Vec<Runner<'plan>>,
    /// The jobs left waiting before any barrier went up.
    older: Vec<Older<'plan>>,
    /// The room behind the barrier, once one stands and has any.
    backfill: Option<Backfill<'plan>>,
}

impl<'plan> Behind<'plan> {
    /// The runners in `profiles`' slots: what one that waits for a job will run is not known.
    fn new(profiles: &'plan BTreeMap<String, Profile>) -> Behind<'plan> {
        let mut runners = Vec::new();
        for (name, profile) in profiles {
            for slot in &profile.slots {
                if let Slot::Runner(runner) = slot {
                    runners.push(Runner {
                        profile: name,
                        started_at: runner.started_at,
                        usual: runner.usual.filter(|_| runner.busy),
                        memory: runner.memory,
                        cpu: runner.cpu,
                        caps: profile.caps(),
                        passed: runner.behind.is_some(),
                    });
                }
            }
        }
        Behind {
            runners,
            older: Vec::new(),
            backfill: None,
        }
    }

    /// Notes `job`, which did not start: it put up `barrier`, or goes before any barrier to come.
    fn left_waiting(
        &mut self,
        job: Older<'plan>,
        barrier: Option<Barrier>,
        left: Left,
        budget: &Budget,
        now: SystemTime,
    ) {
        let Some(barrier) = barrier else {
            self.older.push(job);
            return;
        };
        let around = Around {
            left,
            credit_room: budget.credit_room,
            host_credit_room: budget.host_credit_room,
            drained: budget.drained,
            runners: &self.runners,
            older: &self.older,
        };
        self.backfill = Backfill::behind(barrier, job.profile, around, now);
    }

    /// Whether a runner of `repository` in `profile`, which has `free_slots`, may start behind the barrier, counting it
    /// if so.
    fn admits(&mut self, repository: &Repository, profile: &Profile, free_slots: usize) -> bool {
        let candidate = Candidate {
            profile: &repository.profile,
            free_slots,
            usual: repository.usual,
            caps: profile.caps(),
        };
        let backfill = self.backfill.as_mut();
        backfill.is_some_and(|backfill| backfill.admits(candidate))
    }

    /// Notes a runner started now, behind `barrier` if one stands. It has no job yet: GitHub may give it a shorter one
    /// than its pool's longest, so its room may come sooner than that says.
    fn started(
        &mut self,
        profile: &'plan str,
        caps: Caps,
        memory: u64,
        cpu: u64,
        barrier: Option<Barrier>,
        now: SystemTime,
    ) {
        self.runners.push(Runner {
            profile,
            started_at: now,
            usual: None,
            memory,
            cpu,
            caps,
            passed: barrier.is_some(),
        });
    }
}

/// What `sort_out` leaves the plan to serve.
struct ToServe<'plan> {
    /// How many runners each pool wants for its jobs, by `pool_key`.
    wanted: BTreeMap<(&'plan str, &'plan str), usize>,
    /// Their jobs, oldest first, each with its pool.
    jobs: Vec<(u64, &'plan Repository)>,
    /// How many warm runners each pool is short of.
    warm: Vec<(&'plan Repository, usize)>,
}

/// Sorts out each of `repositories` (`sort_out`) into what the plan serves.
fn to_serve<'plan>(
    repositories: &'plan [Repository],
    profiles: &BTreeMap<String, Profile>,
    now: SystemTime,
    decided: &mut Plan,
) -> ToServe<'plan> {
    let mut serve = ToServe {
        wanted: BTreeMap::new(),
        jobs: Vec::new(),
        warm: Vec::new(),
    };
    for repository in repositories {
        if let Some(sorted) = sort_out(repository, profiles, now, decided) {
            serve
                .wanted
                .insert(pool_key(repository), sorted.serve.len());
            serve
                .jobs
                .extend(sorted.serve.into_iter().map(|job| (job, repository)));
            serve.warm.push((repository, sorted.warm));
        }
    }
    serve.jobs.sort_by_key(|(id, _)| *id);
    serve
}

/// What the jobs a plan has waiting only for a slot would take of the budgets, oldest first
/// (`Plan::slot_waiting`).
#[derive(Debug, Default)]
struct SlotBound {
    memory: u64,
    cpu: u64,
}

impl SlotBound {
    /// Whether a runner of `memory` and `cpu` fits what is `left` beside the older ones it holds, and if so takes it.
    fn takes(&mut self, left: Left, memory: u64, cpu: u64) -> bool {
        let mut spare = left;
        spare.take(self.memory, self.cpu);
        let fits = spare.fits(memory, cpu);
        if fits {
            self.memory = self.memory.saturating_add(memory);
            self.cpu = self.cpu.saturating_add(cpu);
        }
        fits
    }
}

/// The free slot the queued job `job` of `repository` starts in: the one that ran the same job last (repository,
/// workflow and name), else one that ran a job of the same repository, else the lowest. Warm caches are the slot's
/// own layer (ADR 0007), so the same job there finds them. Only free slots count: a start never waits for the one it
/// would prefer. Short of an exact match, a slot that a younger job of the `later` ones in the profile matches
/// exactly is left to it while another is free.
fn free_slot(
    taken: &[bool],
    profile: &Profile,
    repository: &Repository,
    job: u64,
    later: &[(u64, &Repository)],
) -> Option<usize> {
    let ran = |index: usize| profile.ran.get(index).and_then(Option::as_ref);
    let ran_it = |index: usize, repository: &Repository, job: u64| {
        let queued = repository.queued.iter().find(|queued| queued.id == job);
        queued.zip(ran(index)).is_some_and(|(queued, key)| {
            key.repository == repository.name
                && key.workflow == queued.workflow
                && key.name == queued.name
        })
    };
    taken
        .iter()
        .enumerate()
        .filter(|(_, taken)| !**taken)
        .map(|(index, _)| index)
        .min_by_key(|&index| {
            let wanted_later = later.iter().any(|&(id, other)| {
                other.profile == repository.profile && ran_it(index, other, id)
            });
            let same_repository = ran(index).is_some_and(|key| key.repository == repository.name);
            (
                !ran_it(index, repository, job),
                wanted_later,
                !same_repository,
                index,
            )
        })
}

/// Holds `job` of `repository`'s pool for `scarce`, unless the plan already held an older job of the pool for it.
fn hold(repository: &Repository, job: u64, scarce: Scarce, decided: &mut Plan) {
    let pool = repository.pool();
    let held = decided
        .held
        .iter()
        .any(|demand| demand.pool == pool && demand.scarce == scarce);
    if !held {
        decided.held.push(Demand { pool, job, scarce });
    }
}

/// A queued job as `plan`'s loop reaches it, if no runner starts for it.
struct Turn<'plan> {
    repository: &'plan Repository,
    profile: &'plan Profile,
    job: u64,
    memory: u64,
    /// Millicores.
    cpu: u64,
    free_slot: bool,
    left: Left,
    started: &'plan Started<'plan>,
}

impl Turn<'_> {
    /// Why the job waits, and what it lacks.
    fn why(&self) -> (Waiting, Short) {
        why_waits(
            &self.repository.profile,
            self.profile,
            self.free_slot,
            self.memory,
            self.cpu,
            self.left,
            self.started,
        )
    }

    /// Says why the job waits with no barrier up. Lacking memory or CPU it is held, unless a safety interlock left no
    /// `drained` bounds, and it puts up the barrier if it may.
    fn waits(
        self,
        drained: Option<Drained>,
        slot_bound: &mut SlotBound,
        now: SystemTime,
        decided: &mut Plan,
    ) {
        let (reason, short) = self.why();
        decided.waiting.insert(self.job, reason);
        if !self.free_slot
            && self.profile.preparing.is_none()
            && slot_bound.takes(self.left, self.memory, self.cpu)
        {
            decided.slot_waiting.insert(self.job);
        }
        let Some(scarce) = short.scarce() else {
            return;
        };
        if drained.is_some() {
            hold(self.repository, self.job, scarce, decided);
        }
        decided.barrier = Barrier::of(
            priority::Short {
                job: self.job,
                scarce,
                held: short.held(),
                memory: self.memory,
                cpu: self.cpu,
            },
            &self.repository.waits,
            drained,
            now,
        );
    }

    /// Says why the job waits behind `barrier`. A job with no slot keeps saying so, as the barrier is not what holds
    /// it, but counts for no more slots: another one would not start it. One that lacks memory or CPU itself is held
    /// all the same and ages behind the barrier, which it does not share: it may go next.
    fn behind(self, barrier: Barrier, decided: &mut Plan) {
        if self.free_slot && self.left.fits(self.memory, self.cpu) {
            decided.waiting.insert(self.job, barrier.reason());
            return;
        }
        let (reason, short) = self.why();
        let reason = if self.free_slot {
            barrier.reason()
        } else {
            reason
        };
        decided.waiting.insert(self.job, reason);
        if let Some(scarce) = short.scarce() {
            hold(self.repository, self.job, scarce, decided);
        }
    }
}

/// A pool's repository and profile, which tell it apart in a plan.
fn pool_key(repository: &Repository) -> (&str, &str) {
    (&repository.name, &repository.profile)
}

/// Says why each queued job no pool of its repository matches waits, unless GitHub runs it on its own runners.
fn unserved(repositories: &[Repository], decided: &mut Plan) {
    let mut seen = BTreeSet::new();
    for repository in repositories {
        if !seen.insert(repository.name.as_str()) {
            continue;
        }
        let pools: Vec<&Repository> = repositories
            .iter()
            .filter(|pool| pool.name == repository.name)
            .collect();
        decided.waiting.extend(
            repository
                .queued
                .iter()
                .filter(|job| {
                    !hosted_by_github(job) && !pools.iter().any(|pool| matches(job, &pool.labels))
                })
                .map(|job| {
                    (
                        job.id,
                        Waiting::NoPool {
                            labels: job.labels.clone(),
                        },
                    )
                }),
        );
    }
}

/// What a job's runner lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Short {
    Slot,
    Memory,
    Host,
    Cpu,
}

impl Short {
    /// What a running job gives back of it: not a slot, which a job waits for in turn.
    fn scarce(self) -> Option<Scarce> {
        match self {
            Short::Slot => None,
            Short::Memory | Short::Host => Some(Scarce::Memory),
            Short::Cpu => Some(Scarce::Cpu),
        }
    }

    /// Whether the host is what holds the job.
    fn held(self) -> Held {
        match self {
            Short::Host => Held::ByHost,
            Short::Slot | Short::Memory | Short::Cpu => Held::ByBudget,
        }
    }
}

/// Why a job whose runner of `memory` and `cpu` does not fit in `profile` (named `name`) waits, given whether it has
/// a `free_slot` and the budgets `left`, and what it lacks.
fn why_waits(
    name: &str,
    profile: &Profile,
    free_slot: bool,
    memory: u64,
    cpu: u64,
    left: Left,
    started: &Started<'_>,
) -> (Waiting, Short) {
    let profile_name = || name.to_owned();
    if !free_slot {
        let reason = match profile.preparing {
            Some(Preparing::Reseeding) => Waiting::Reseeding {
                profile: profile_name(),
            },
            Some(Preparing::PullingImage) => Waiting::PullingImage {
                profile: profile_name(),
            },
            None => started.for_slot(
                name,
                Waiting::NoSlot {
                    profile: profile_name(),
                },
            ),
        };
        return (reason, Short::Slot);
    }
    match (left.memory, left.host, left.cpu) {
        (Some(free), _, _) if free < signed(memory) => (
            started.for_memory(
                memory,
                Waiting::Memory {
                    needs: memory,
                    free: unsigned(free),
                },
            ),
            Short::Memory,
        ),
        (_, Some(free), _) if free < signed(memory) => (
            started.for_host(
                memory,
                Waiting::HostMemory {
                    needs: memory,
                    free: unsigned(free),
                },
            ),
            Short::Host,
        ),
        (_, _, Some(free)) => (
            started.for_cpu(
                cpu,
                Waiting::Cpu {
                    needs: cpu,
                    free: unsigned(free),
                },
            ),
            Short::Cpu,
        ),
        // `fits` said otherwise.
        (_, _, None) => (
            Waiting::NoSlot {
                profile: profile_name(),
            },
            Short::Slot,
        ),
    }
}

/// A runner of `repository` in slot `index` of its profile for queued `job`, after the warm runners it `replaces`
/// stop.
fn start(
    repository: &Repository,
    job: u64,
    index: usize,
    memory: u64,
    cpu: u64,
    replacing: Vec<SlotRef>,
    barrier: Option<Barrier>,
) -> Decision {
    Decision::Start {
        slot: SlotRef {
            profile: repository.profile.clone(),
            index,
        },
        repository: repository.name.clone(),
        memory,
        cpu,
        for_: StartFor::Job(job),
        replacing,
        behind: barrier.map(|barrier| barrier.job),
    }
}

/// Starts the warm runners each repository is short of (`wanted`), at its profile's caps, in the slots and budget
/// the jobs `left`: one per repository at a time, so a few free slots go to several repositories rather than to the
/// first. None while a job waits for room they would take.
fn start_warm(
    wanted: &[(&Repository, usize)],
    profiles: &BTreeMap<String, Profile>,
    taken: &mut BTreeMap<&str, Vec<bool>>,
    mut left: Left,
    decided: &mut Plan,
) {
    let room_wanted = decided.waiting.values().any(|reason| {
        matches!(
            reason,
            Waiting::NoSlot { .. }
                | Waiting::Memory { .. }
                | Waiting::HostMemory { .. }
                | Waiting::Cpu { .. }
                | Waiting::BehindCpu { .. }
                | Waiting::BehindMemory { .. }
                | Waiting::BehindStart { .. }
        )
    });
    if room_wanted {
        return;
    }
    for round in 0.. {
        let mut any = false;
        for (repository, wants) in wanted {
            if *wants <= round {
                continue;
            }
            let (Some(profile), Some(taken)) = (
                profiles.get(&repository.profile),
                taken.get_mut(repository.profile.as_str()),
            ) else {
                continue;
            };
            let (memory, cpu) = (profile.memory, profile.cpus);
            let Some(index) = taken.iter().position(|taken| !taken) else {
                continue;
            };
            if !left.fits(memory, cpu) {
                continue;
            }
            taken[index] = true;
            left.take(memory, cpu);
            any = true;
            decided.decisions.push(Decision::Start {
                slot: SlotRef {
                    profile: repository.profile.clone(),
                    index,
                },
                repository: repository.name.clone(),
                memory,
                cpu,
                for_: StartFor::Warm,
                replacing: Vec::new(),
                behind: None,
            });
        }
        if !any {
            break;
        }
    }
}

/// The memory, CPU and host budgets a plan has left; `None`, no budget. Each is negative while what runs takes past
/// it: a stopped runner's reservation pays that back first, so a start never takes room that is owed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Left {
    pub(crate) memory: Option<i64>,
    pub(crate) cpu: Option<i64>,
    /// The host's memory to spare, which a runner's memory takes from as it does from `memory`.
    pub(crate) host: Option<i64>,
}

impl Profile {
    /// What a runner of this profile could take.
    fn caps(&self) -> Caps {
        Caps {
            memory: self.memory,
            cpu: self.cpus,
        }
    }
}

impl Left {
    fn fits(self, memory: u64, cpu: u64) -> bool {
        let fits = |free: Option<i64>, needs: u64| free.is_none_or(|free| free >= signed(needs));
        fits(self.memory, memory) && fits(self.host, memory) && fits(self.cpu, cpu)
    }

    /// What a runner of `memory` and `cpu` lacks, the host's memory counted with the slice's: none where there is
    /// no budget.
    fn short(self, memory: u64, cpu: u64) -> Left {
        let lacks = |free: Option<i64>, needs: u64| {
            free.map(|free| signed(needs).saturating_sub(free).max(0))
        };
        Left {
            memory: lacks(self.memory, memory).max(lacks(self.host, memory)),
            cpu: lacks(self.cpu, cpu),
            host: None,
        }
    }

    fn take(&mut self, memory: u64, cpu: u64) {
        let take = |free: &mut Option<i64>, used: u64| {
            *free = free.map(|free| free.saturating_sub(signed(used)));
        };
        take(&mut self.memory, memory);
        take(&mut self.host, memory);
        take(&mut self.cpu, cpu);
    }

    fn give(&mut self, memory: u64, cpu: u64) {
        let give = |free: &mut Option<i64>, freed: u64| {
            *free = free.map(|free| free.saturating_add(signed(freed)));
        };
        give(&mut self.memory, memory);
        give(&mut self.host, memory);
        give(&mut self.cpu, cpu);
    }
}

/// What `budget` leaves beside what runs `in_use`: negative by what runs past it, so a deficit stays owed rather
/// than hidden at 0.
fn left(budget: u64, in_use: u64) -> i64 {
    signed(budget).saturating_sub(signed(in_use))
}

/// A budget's bytes or millicores, signed as `Left` counts them.
fn signed(amount: u64) -> i64 {
    i64::try_from(amount).unwrap_or(i64::MAX)
}

/// A signed budget as a job's wait shows it: none once it is spent.
fn unsigned(free: i64) -> u64 {
    u64::try_from(free).unwrap_or(0)
}

/// What `sort_out` leaves a repository to start: runners for the IDs of its jobs, and warm runners.
struct Sorted {
    serve: Vec<u64>,
    warm: usize,
}

/// The queued jobs `repository`'s labels match, oldest first whatever the listing's order: the order `to_serve` starts
/// runners in, so the job a start served is the one its runner is named for at the next plan (ADR 0110).
fn oldest_first(repository: &Repository) -> Vec<&QueuedJob> {
    let mut matching: Vec<&QueuedJob> = repository
        .queued
        .iter()
        .filter(|job| matches(job, &repository.labels))
        .collect();
    matching.sort_by_key(|job| job.id);
    matching
}

/// Stops a repository's idle runners no job is left for, beyond its warm ones, and says why its jobs wait when it is
/// paused or held, or which an idle runner will take. Returns the jobs it wants a runner for and how many warm
/// runners it is short of, unless it is held.
fn sort_out(
    repository: &Repository,
    profiles: &BTreeMap<String, Profile>,
    now: SystemTime,
    decided: &mut Plan,
) -> Option<Sorted> {
    let mut matching = oldest_first(repository);
    let profile = profiles.get(&repository.profile);
    let mut warm = repository.warm;
    if repository.paused {
        decided
            .waiting
            .extend(matching.iter().map(|job| (job.id, Waiting::Paused)));
        // None is left for its idle runners, which stop at once.
        matching.clear();
        warm = 0;
    }
    // Upkeep waits for its profile's runners: none is kept, and none waits out `IDLE_TIMEOUT` first. Nor does a
    // paused repository's: GitHub would give it a job queued before the pause.
    let upkeep_due = profile.is_some_and(|profile| profile.upkeep_due);
    let stop_now = upkeep_due || repository.paused;
    if upkeep_due {
        warm = 0;
    }
    let (idle, closing): (Vec<(SlotRef, &RunnerSlot)>, Vec<_>) = profile
        .into_iter()
        .flat_map(|profile| profile.slots.iter().enumerate())
        .filter_map(|(index, slot)| match slot {
            Slot::Runner(runner) if runner.repository == repository.name && !runner.busy => Some((
                SlotRef {
                    profile: repository.profile.clone(),
                    index,
                },
                runner,
            )),
            _ => None,
        })
        .partition(|(slot, _)| profile.is_none_or(|profile| slot.index < profile.count));
    // Idle runners in slots a lowered `slots` closed stop now: neither warm nor room for a job, their slots close.
    decided
        .decisions
        .extend(closing.into_iter().map(|(slot, _)| Decision::Stop {
            slot,
            reason: StopReason::Closed,
        }));
    let surplus = idle.len().saturating_sub(matching.len());
    let stops: Vec<SlotRef> = idle
        .iter()
        .filter(|(_, runner)| {
            stop_now
                || now
                    .duration_since(runner.started_at)
                    .is_ok_and(|waited| waited >= IDLE_TIMEOUT)
        })
        .take(surplus.saturating_sub(warm))
        .map(|(slot, _)| slot.clone())
        .collect();
    // The idle runners left past those its jobs take are warm.
    let kept: Vec<&SlotRef> = idle
        .iter()
        .map(|(slot, _)| slot)
        .filter(|slot| !stops.contains(slot))
        .collect();
    decided
        .warm
        .extend(kept.iter().skip(matching.len()).map(|slot| (*slot).clone()));
    let warm = warm.saturating_sub(kept.len().saturating_sub(matching.len()));
    decided
        .decisions
        .extend(stops.into_iter().map(|slot| Decision::Stop {
            slot,
            reason: if repository.paused {
                StopReason::Paused
            } else if upkeep_due {
                StopReason::Upkeep
            } else {
                StopReason::Expired
            },
        }));
    let missing = matching.len().saturating_sub(idle.len());
    // The idle runners take the oldest jobs; the younger ones still wait.
    let skip = matching.len() - missing;
    decided.waiting.extend(
        matching
            .iter()
            .take(skip)
            .map(|job| (job.id, Waiting::RunnerStarting)),
    );
    let serve = matching.iter().skip(skip).map(|job| job.id);
    match repository.held.filter(|held| held.until > now) {
        Some(held) => {
            let until = crate::time::ms(held.until) / 1000;
            decided.waiting.extend(serve.map(|job| {
                (
                    job,
                    Waiting::Backoff {
                        until,
                        cause: held.cause,
                    },
                )
            }));
            None
        }
        None => Some(Sorted {
            serve: serve.collect(),
            warm,
        }),
    }
}

/// The warm runners a queued job may stop to make room for itself.
struct Warm<'plan> {
    /// Each warm runner with what it takes, in slot order; `None` once stopped for a job.
    runners: Vec<Option<(SlotRef, &'plan RunnerSlot)>>,
}

/// The room some warm runners give back.
struct Room {
    /// The warm runners to stop.
    stopped: Vec<SlotRef>,
    /// A slot of the job's profile among them, when it needed one.
    slot: Option<usize>,
    memory: u64,
    cpu: u64,
}

impl<'plan> Warm<'plan> {
    /// The warm runners of `warm`; none while the slice is `held`, when no start may use their room.
    fn new(
        warm: &BTreeSet<SlotRef>,
        profiles: &'plan BTreeMap<String, Profile>,
        held: bool,
    ) -> Warm<'plan> {
        let runners = warm
            .iter()
            .filter(|_| !held)
            .filter_map(
                |slot| match profiles.get(&slot.profile)?.slots.get(slot.index)? {
                    Slot::Runner(runner) => Some(Some((slot.clone(), runner))),
                    Slot::Free | Slot::Closed => None,
                },
            )
            .collect();
        Warm { runners }
    }

    /// Picks warm runners of pools other than `repository` whose room lets its next runner, of `needs_memory` and
    /// `needs_cpu`, start in the budgets `left`: a slot of its profile when it has none (`slot`), and the memory
    /// and CPU it is short of. Those in its profile go first. `None`, and nothing picked, when all of them would
    /// not do.
    fn make_room(
        &mut self,
        repository: &Repository,
        slot: bool,
        left: Left,
        needs_memory: u64,
        needs_cpu: u64,
    ) -> Option<Room> {
        let short = left.short(needs_memory, needs_cpu);
        let (memory, cpu) = (
            unsigned(short.memory.unwrap_or(0)),
            unsigned(short.cpu.unwrap_or(0)),
        );
        let mut candidates: Vec<usize> = self
            .runners
            .iter()
            .enumerate()
            .filter_map(|(at, runner)| {
                runner
                    .as_ref()
                    .filter(|(slot, runner)| {
                        runner.repository != repository.name || slot.profile != repository.profile
                    })
                    .map(|_| at)
            })
            .collect();
        // Its own profile's first: their slots serve it too.
        candidates.sort_by_key(|at| {
            self.runners[*at]
                .as_ref()
                .is_some_and(|(slot, _)| slot.profile != repository.profile)
        });
        let mut room = Room {
            stopped: Vec::new(),
            slot: None,
            memory: 0,
            cpu: 0,
        };
        let mut picked = Vec::new();
        for at in candidates {
            let short = (slot && room.slot.is_none()) || room.memory < memory || room.cpu < cpu;
            if !short {
                break;
            }
            let Some((ref_, runner)) = &self.runners[at] else {
                continue;
            };
            let gives_slot = ref_.profile == repository.profile;
            // A runner that gives nothing the job still lacks stays.
            let helps = (gives_slot && slot && room.slot.is_none())
                || (room.memory < memory && runner.memory > 0)
                || (room.cpu < cpu && runner.cpu > 0);
            if !helps {
                continue;
            }
            if gives_slot && room.slot.is_none() {
                room.slot = Some(ref_.index);
            }
            room.memory += runner.memory;
            room.cpu += runner.cpu;
            room.stopped.push(ref_.clone());
            picked.push(at);
        }
        // What they give back must let it start: a start never takes what the stops do not free.
        let mut after = left;
        after.give(room.memory, room.cpu);
        if (slot && room.slot.is_none()) || !after.fits(needs_memory, needs_cpu) {
            return None;
        }
        for at in picked {
            self.runners[at] = None;
        }
        Some(room)
    }
}

/// What a plan's own starts took, so a job held only by them waits behind them rather than for a slot, memory or CPU
/// in use. The status explains with a plan it does not run, whose starts may never happen.
struct Started<'plan> {
    /// The profiles with a free slot before the plan's starts.
    free_slots: BTreeSet<&'plan str>,
    /// The budgets left before them.
    before: Left,
    /// The first job each profile, and any profile, starts a runner for.
    by_profile: BTreeMap<&'plan str, u64>,
    first: Option<u64>,
}

impl<'plan> Started<'plan> {
    fn new(taken: &BTreeMap<&'plan str, Vec<bool>>, before: Left) -> Started<'plan> {
        Started {
            free_slots: taken
                .iter()
                .filter(|(_, slots)| slots.contains(&false))
                .map(|(name, _)| *name)
                .collect(),
            before,
            by_profile: BTreeMap::new(),
            first: None,
        }
    }

    /// The plan starts a runner for `job` in `profile`.
    fn note(&mut self, profile: &'plan str, job: u64) {
        self.by_profile.entry(profile).or_insert(job);
        self.first.get_or_insert(job);
    }

    /// `reason` for a job `profile` has no slot for, unless only this plan's starts took its free slots.
    fn for_slot(&self, profile: &str, reason: Waiting) -> Waiting {
        match self.by_profile.get(profile) {
            Some(older) if self.free_slots.contains(profile) => {
                Waiting::BehindStart { job: *older }
            }
            _ => reason,
        }
    }

    /// `reason` for a job whose `needs` of memory do not fit, unless they did before this plan's starts.
    fn for_memory(&self, needs: u64, reason: Waiting) -> Waiting {
        Self::behind(self.first, self.before.memory, needs).unwrap_or(reason)
    }

    /// `reason` for a job whose `needs` of memory the host cannot spare, unless it could before this plan's starts.
    fn for_host(&self, needs: u64, reason: Waiting) -> Waiting {
        Self::behind(self.first, self.before.host, needs).unwrap_or(reason)
    }

    /// `reason` for a job whose `needs` of CPU do not fit, unless they did before this plan's starts.
    fn for_cpu(&self, needs: u64, reason: Waiting) -> Waiting {
        Self::behind(self.first, self.before.cpu, needs).unwrap_or(reason)
    }

    fn behind(first: Option<u64>, free: Option<i64>, needs: u64) -> Option<Waiting> {
        first
            .filter(|_| free.is_some_and(|free| free >= signed(needs)))
            .map(|job| Waiting::BehindStart { job })
    }
}

/// Whether each slot of each profile is taken, by profile name: by a runner, closed, or its profile preparing.
fn taken_slots(profiles: &BTreeMap<String, Profile>) -> BTreeMap<&str, Vec<bool>> {
    profiles
        .iter()
        .map(|(name, profile)| {
            let taken = profile
                .slots
                .iter()
                .map(|slot| profile.preparing.is_some() || *slot != Slot::Free)
                .collect();
            (name.as_str(), taken)
        })
        .collect()
}

#[cfg(test)]
mod tests;
