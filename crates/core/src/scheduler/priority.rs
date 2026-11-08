//! Which one pool goes first once memory or CPU keeps running out before its next job's runner fits (ADR 0096): how
//! long each pool's next job waited, whether a barrier has served the pool (ADR 0098), what an empty slice would hold,
//! the rule that picks a plan's one barrier, and which younger jobs may start behind it all the same (`Backfill`, ADR
//! 0097). `plan` and `admission::simulate` both decide by it, so a replay shows what the daemon does. It does no I/O
//! and reads no clock; the engine keeps the waits, and it never stops or clamps what already runs.

use std::time::{Duration, SystemTime};

use super::{Left, signed};
use crate::api::Waiting;

/// Once a pool's next job has waited this long for memory or CPU, no younger job starts until its runner fits: smaller
/// jobs would otherwise take everything that frees up, and the room a large one needs would never gather. Long enough
/// that a passing shortage puts up no barrier, whose cost is what stands idle behind it (ADR 0098).
pub const PRIORITY_AFTER: Duration = Duration::from_secs(600);

/// `PRIORITY_AFTER` for a pool a barrier has served, until its queue empties: its wait restarts with each job, so with
/// the first limit a pool of many large jobs could run them only one at a time (ADR 0098).
pub const SERVED_PRIORITY_AFTER: Duration = Duration::from_secs(120);

/// What a job's runner lacks that a running job gives back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scarce {
    /// The slice's memory budget, or what the host's memory leaves it.
    Memory,
    /// The CPU budget.
    Cpu,
}

/// Since when a job has waited for something scarce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    /// The job's ID: a wait counts for this job alone, so it cannot outlive it.
    pub job: u64,
    /// When a scheduling plan first held it.
    pub since: SystemTime,
}

/// What a pool's next job has waited for, and whether a barrier has served the pool. Each clock runs from the first plan
/// that held the job for it and goes on through a plan that holds it for the other, or for a slot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Waits {
    /// Its wait for memory.
    pub memory: Option<Wait>,
    /// Its wait for CPU.
    pub cpu: Option<Wait>,
    /// The waiting job, if a plan's barrier was ever its own. Not only the last plan's: the plan that starts it has no
    /// barrier, as it fits, and a start refused, or that failed preparing its caches, plans again. A failure that brings
    /// a backoff clears this with the waits.
    pub owner: Option<u64>,
    /// Whether a runner of the pool started for a job that owned a barrier, since its queue last emptied: its next jobs
    /// wait `SERVED_PRIORITY_AFTER`.
    pub served: bool,
}

impl Waits {
    /// Whether nothing is kept for the pool.
    pub fn is_empty(&self) -> bool {
        *self == Waits::default()
    }

    /// Starts the clock of `scarce` for `job`, unless one runs; whether it started.
    pub fn note(&mut self, scarce: Scarce, job: u64, now: SystemTime) -> bool {
        let wait = match scarce {
            Scarce::Memory => &mut self.memory,
            Scarce::Cpu => &mut self.cpu,
        };
        let started = wait.is_none();
        wait.get_or_insert(Wait { job, since: now });
        started
    }

    /// Drops what is kept for the jobs `queued` says are gone. The mark stays: a job taken elsewhere says nothing of the
    /// jobs behind it.
    pub fn retain(&mut self, queued: impl Fn(u64) -> bool) {
        for wait in [&mut self.memory, &mut self.cpu] {
            if wait.is_some_and(|wait| !queued(wait.job)) {
                *wait = None;
            }
        }
        self.owner = self.owner.filter(|job| queued(*job));
    }

    /// Notes that the plan's barrier is `job`'s, if this pool waits for it.
    pub fn own(&mut self, job: u64) {
        let waits = [self.memory, self.cpu];
        if waits.iter().flatten().any(|wait| wait.job == job) {
            self.owner = Some(job);
        }
    }

    /// A runner of the pool is in its slot: its waits end, and if it came to a job that owned a barrier, the barrier has
    /// served the pool. A proposed start, a deferred one or one that failed comes to nothing, so it does not count.
    pub fn started(&mut self) {
        self.served |= self.owner.is_some();
        *self = Waits {
            served: self.served,
            ..Waits::default()
        };
    }

    /// A listing that succeeded shows none of the pool's jobs queued: the next job to wait is a new contention.
    pub fn emptied(&mut self) {
        self.served = false;
    }

    /// How long a job of the pool waits before it may own a barrier.
    fn limit(&self) -> Duration {
        if self.served {
            SERVED_PRIORITY_AFTER
        } else {
            PRIORITY_AFTER
        }
    }

    /// When each wait reaches its limit, for a replay to look again then.
    pub fn deadlines(&self) -> impl Iterator<Item = SystemTime> {
        let limit = self.limit();
        [self.memory, self.cpu]
            .into_iter()
            .flatten()
            .map(move |wait| wait.since + limit)
    }

    /// When `job` first had waited long enough for either, if it has by `now`: a job that waited for CPU and now lacks
    /// memory, or the other way round, keeps what it waited.
    fn matured(&self, job: u64, now: SystemTime) -> Option<SystemTime> {
        let limit = self.limit();
        [self.memory, self.cpu]
            .into_iter()
            .flatten()
            .filter(|wait| wait.job == job)
            .map(|wait| wait.since + limit)
            .filter(|reached| *reached <= now)
            .min()
    }
}

#[cfg(test)]
impl Waits {
    /// With a wait for memory, for tests that start from one.
    pub(crate) fn with_memory(mut self, wait: impl Into<Option<Wait>>) -> Self {
        self.memory = wait.into();
        self
    }

    /// With a wait for CPU, for tests that start from one.
    pub(crate) fn with_cpu(mut self, wait: impl Into<Option<Wait>>) -> Self {
        self.cpu = wait.into();
        self
    }

    /// With a barrier having served the pool, or not.
    pub(crate) fn with_served(mut self, served: bool) -> Self {
        self.served = served;
        self
    }
}

/// What the budgets would leave once every runner ended, the mirror, seed containers and shells still there; `None`,
/// no such bound known. A job holds younger ones only for room that draining can give it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drained {
    /// The slice's memory.
    pub memory: Option<i64>,
    /// The CPU budget, in millicores.
    pub cpu: Option<i64>,
    /// What the host's memory leaves the slice for a job the host holds: by the larger margin while a runner runs,
    /// whichever margin is in force (ADR 0102).
    pub host: Option<i64>,
    /// What it leaves for a job something else holds, by the plain margin: such a job's wait does not follow the margin,
    /// so its barrier need not keep off it.
    pub host_plain: Option<i64>,
}

impl Drained {
    /// Whether a runner of `memory` and `cpu`, of a job `held` so, fits, every known bound counted.
    fn holds(self, memory: u64, cpu: u64, held: Held) -> bool {
        let fits = |room: Option<i64>, needs: u64| {
            room.is_none_or(|room| room >= i64::try_from(needs).unwrap_or(i64::MAX))
        };
        let host = match held {
            Held::ByHost => self.host,
            Held::ByBudget => self.host_plain,
        };
        fits(self.memory, memory) && fits(host, memory) && fits(self.cpu, cpu)
    }
}

/// What holds a job back, which says by which margin a drain serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// What the host's memory leaves the slice.
    ByHost,
    /// The slice's memory budget, its high-water mark or the CPU budget.
    ByBudget,
}

/// A job a plan held for something scarce, with a free slot for its runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Short {
    /// Its ID.
    pub job: u64,
    /// What it lacks first: memory before CPU, as `Waiting` says it.
    pub scarce: Scarce,
    /// Whether the host holds it.
    pub held: Held,
    /// What its runner takes of the memory budget.
    pub memory: u64,
    /// What its runner takes of the CPU budget, in millicores.
    pub cpu: u64,
}

/// The one job a plan lets no younger job start before, whichever of memory and CPU it lacks: it drains towards both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Barrier {
    /// The job's ID.
    pub job: u64,
    /// What it lacks now.
    pub scarce: Scarce,
    /// What its runner takes of the memory budget.
    pub memory: u64,
    /// What its runner takes of the CPU budget, in millicores.
    pub cpu: u64,
    /// When its wait reached its limit: from then on, what starts counts against the room it is owed.
    pub since: SystemTime,
}

impl Barrier {
    /// The barrier `short`, whose pool `waits`, puts up: the first of a plan's jobs that waited long enough and that
    /// `drained` has room for. `None` from a demand draining could not serve, as a runner past the budget or a host
    /// short because of programs outside the slice: younger jobs that fit go on. `drained` is `None` while a safety
    /// interlock starts nothing, whose zeros say nothing of the room there is: the wait keeps its age meanwhile.
    pub fn of(
        short: Short,
        waits: &Waits,
        drained: Option<Drained>,
        now: SystemTime,
    ) -> Option<Barrier> {
        let drained = drained?;
        let since = waits.matured(short.job, now)?;
        let holds = drained.holds(short.memory, short.cpu, short.held);
        holds.then_some(Barrier {
            job: short.job,
            scarce: short.scarce,
            memory: short.memory,
            cpu: short.cpu,
            since,
        })
    }

    /// Why a younger job waits behind it.
    pub fn reason(self) -> Waiting {
        match self.scarce {
            Scarce::Memory => Waiting::BehindMemory { job: self.job },
            Scarce::Cpu => Waiting::BehindCpu { job: self.job },
        }
    }
}

/// What a runner may take at most: its profile's caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caps {
    /// Memory.
    pub(crate) memory: u64,
    /// CPU, in millicores.
    pub(crate) cpu: u64,
}

/// A runner as a backfill reads it, one in a slot or one the plan already started.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Runner<'plan> {
    /// Its profile.
    pub(crate) profile: &'plan str,
    /// When it started.
    pub(crate) started_at: SystemTime,
    /// How long its job usually keeps it; `None`, it ends now as far as a backfill knows: it runs no job yet, or one
    /// with no estimate. That only makes the room come sooner, and less starts behind the barrier.
    pub(crate) usual: Option<Duration>,
    /// What it takes of the memory budget now.
    pub(crate) memory: u64,
    /// What it takes of the CPU budget now, in millicores.
    pub(crate) cpu: u64,
    /// What it could take.
    pub(crate) caps: Caps,
}

impl Runner<'_> {
    /// How long from `now` it usually ends: at once if that time has passed, or if it started in the future, as after
    /// a clock went back: the room coming sooner is the side that starts less behind a barrier.
    fn left_to_run(&self, now: SystemTime) -> Duration {
        let Ok(ran) = now.duration_since(self.started_at) else {
            return Duration::ZERO;
        };
        self.usual
            .map_or(Duration::ZERO, |usual| usual.saturating_sub(ran))
    }
}

/// A job older than a barrier's owner that the plan left waiting. It goes before the owner, so room it loses to a
/// younger job comes back to the owner late.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Older<'plan> {
    /// Its pool's profile.
    pub(crate) profile: &'plan str,
    /// Whether that profile has a free slot, or will have one with no runner ending, as when it is preparing.
    pub(crate) free_slot: bool,
    /// What its runner takes of the memory budget.
    pub(crate) memory: u64,
    /// What its runner takes of the CPU budget, in millicores.
    pub(crate) cpu: u64,
}

/// What a plan knows when a barrier goes up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Around<'plan> {
    /// The budgets left.
    pub(crate) left: Left,
    /// How much more memory the pooled count could leave with no runner ending (`Budget::credit_room`).
    pub(crate) credit_room: u64,
    /// How much more the host's limit could leave with no runner ending (`Budget::host_credit_room`).
    pub(crate) host_credit_room: u64,
    /// What a drain leaves.
    pub(crate) drained: Drained,
    /// The runners in slots, and those the plan started so far.
    pub(crate) runners: &'plan [Runner<'plan>],
    /// The jobs older than the owner it left waiting.
    pub(crate) older: &'plan [Older<'plan>],
}

/// A younger job a plan would start, had it no barrier to wait behind.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'plan> {
    /// Its pool's profile.
    pub(crate) profile: &'plan str,
    /// How many of that profile's slots are free.
    pub(crate) free_slots: usize,
    /// The largest usual run time of its pool's queued jobs, since GitHub may give the runner any of them; `None` if
    /// any has no estimate (`admission::Predictor::longest_usual`).
    pub(crate) usual: Option<Duration>,
    /// Its profile's caps.
    pub(crate) caps: Caps,
}

/// The room behind a barrier, as ADR 0097 bounds it. If each run takes what the plan read, everything started here has
/// ended before the owner or any job older than it could start, so nothing they see changes and an owner that keeps the
/// barrier starts no later than with no backfill. The plan reads p95s, so a run that ends early can bring the room while
/// a job started here still runs, and that job can cost the owner what it has left. Whatever the run times do, what
/// started since the barrier matured cannot take, even at its caps, what an empty slice has for the owner and the older
/// jobs, nor the last free slot of its profile, so the owner fits once the older jobs and what ran before have ended.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Backfill<'plan> {
    /// The owner's profile.
    profile: &'plan str,
    /// How long from now until the running runners' usual ends let the owner or an older job start.
    until: Duration,
    /// What a drain leaves past the owner and the caps of what started since its barrier matured.
    spare: Left,
}

/// The runners by how long each usually has left, soonest first.
struct Ending<'plan>(Vec<(Duration, Runner<'plan>)>);

impl Ending<'_> {
    /// How long until their ends let a runner of `memory` and `cpu` fit `left`; `None` if none does.
    fn room_in(&self, left: Left, memory: u64, cpu: u64) -> Option<Duration> {
        if left.fits(memory, cpu) {
            return Some(Duration::ZERO);
        }
        let mut room = left;
        self.0.iter().find_map(|(left_to_run, runner)| {
            room.give(runner.memory, runner.cpu);
            room.fits(memory, cpu).then_some(*left_to_run)
        })
    }

    /// How long until `older` could start, the budgets being `left`; `None` if no end lets it. Older jobs beside it may
    /// well take that room first, so it may start later, never sooner.
    fn start_of(&self, older: &Older<'_>, left: Left) -> Option<Duration> {
        let room = self.room_in(left, older.memory, older.cpu)?;
        let in_profile = |(_, runner): &&(Duration, Runner<'_>)| runner.profile == older.profile;
        let slot = if older.free_slot {
            Duration::ZERO
        } else {
            self.0.iter().find(in_profile)?.0
        };
        Some(room.max(slot))
    }
}

impl<'plan> Backfill<'plan> {
    /// What the `barrier` of a job of `profile` leaves room for, as things stand `around` it. `None` where nothing may
    /// start behind it: the owner's room comes now, or no end of a running runner brings it, as when something other
    /// than runners holds it.
    pub(crate) fn behind(
        barrier: Barrier,
        profile: &'plan str,
        around: Around<'_>,
        now: SystemTime,
    ) -> Option<Backfill<'plan>> {
        let Around {
            left,
            credit_room,
            host_credit_room,
            drained,
            runners,
            older,
        } = around;
        // The room may come sooner than ends bring it: either credit can grow at any moment, and an end gives back at
        // most what its runner counts. So the time found is the earliest the owner could start. A runner this plan
        // started has no job yet, so it counts as ending now and gives back its whole take at once: what it may later
        // add to the host's credit is its headroom, never more than that take, so `host_credit_room` need not count it.
        let mut left = left;
        left.memory = left
            .memory
            .map(|memory| memory.saturating_add(signed(credit_room)));
        left.host = left
            .host
            .map(|host| host.saturating_add(signed(host_credit_room)));
        let mut ending: Vec<(Duration, Runner<'_>)> = runners
            .iter()
            .map(|runner| (runner.left_to_run(now), *runner))
            .collect();
        ending.sort_by_key(|(left_to_run, _)| *left_to_run);
        let ending = Ending(ending);
        let owner = ending.room_in(left, barrier.memory, barrier.cpu)?;
        let starts = older.iter().filter_map(|job| ending.start_of(job, left));
        let until = starts.fold(owner, Duration::min);
        if until.is_zero() {
            return None;
        }
        let mut spare = Left {
            memory: drained.memory,
            cpu: drained.cpu,
            host: drained.host,
        };
        // An older job that matures takes the barrier, so its room is kept as the owner's is. One no drain serves never
        // will, and must not stop everything behind the barrier.
        let served = older.iter().filter(|job| spare.fits(job.memory, job.cpu));
        let memory = served.clone().map(|job| job.memory).max().unwrap_or(0);
        let cpu = served.map(|job| job.cpu).max().unwrap_or(0);
        spare.take(barrier.memory.max(memory), barrier.cpu.max(cpu));
        for runner in runners
            .iter()
            .filter(|runner| runner.started_at >= barrier.since)
        {
            spare.take(runner.caps.memory, runner.caps.cpu);
        }
        Some(Backfill {
            profile,
            until,
            spare,
        })
    }

    /// Whether `candidate`, which fits the budgets left and has a free slot, may start behind the barrier; if so, counts
    /// its caps. It need not move the time found: a runner that usually ends by then gives back what it took.
    pub(crate) fn admits(&mut self, candidate: Candidate<'_>) -> bool {
        let caps = candidate.caps;
        let admitted = candidate.usual.is_some_and(|usual| usual <= self.until)
            && self.spare.fits(caps.memory, caps.cpu)
            && (candidate.profile != self.profile || candidate.free_slots > 1);
        if admitted {
            self.spare.take(caps.memory, caps.cpu);
        }
        admitted
    }
}
