//! Which one pool goes first once memory or CPU keeps running out before its next job's runner fits (ADR 0096): how
//! long each pool's next job waited, what an empty slice would hold, the rule that picks a plan's one barrier, and
//! which younger jobs may start behind it all the same (`Backfill`, ADR 0097). `plan` and `admission::simulate` both
//! decide by it, so a replay shows what the daemon does. It does no I/O and reads no clock; the engine keeps the waits,
//! and it never stops or clamps what already runs.

use std::time::{Duration, SystemTime};

use super::Left;
use crate::api::Waiting;

/// Once a pool's next job has waited this long for CPU, no younger job starts until its runner fits: smaller jobs
/// arriving all the time would otherwise take every CPU that frees up before it.
pub const CPU_PRIORITY_AFTER: Duration = Duration::from_secs(120);

/// Once a pool's next job has waited this long for memory, no younger job starts until its runner fits: smaller
/// runners would otherwise take every byte that frees up, and the room a large one needs would never gather. Long
/// enough that a job waiting out one short run holds nothing back, short against the hour a starved job waited.
pub const MEMORY_PRIORITY_AFTER: Duration = Duration::from_secs(120);

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

/// What a pool's next job has waited for. Each clock runs from the first plan that held the job for it and goes on
/// through a plan that holds it for the other, or for a slot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Waits {
    /// Its wait for memory.
    pub memory: Option<Wait>,
    /// Its wait for CPU.
    pub cpu: Option<Wait>,
}

impl Waits {
    /// Whether no job waits.
    pub fn is_empty(&self) -> bool {
        self.memory.is_none() && self.cpu.is_none()
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

    /// Drops the waits of the jobs `queued` says are gone.
    pub fn retain(&mut self, queued: impl Fn(u64) -> bool) {
        for wait in [&mut self.memory, &mut self.cpu] {
            if wait.is_some_and(|wait| !queued(wait.job)) {
                *wait = None;
            }
        }
    }

    /// When each wait reaches its limit, for a replay to look again then.
    pub fn deadlines(&self) -> impl Iterator<Item = SystemTime> {
        let memory = self.memory.map(|wait| wait.since + MEMORY_PRIORITY_AFTER);
        let cpu = self.cpu.map(|wait| wait.since + CPU_PRIORITY_AFTER);
        memory.into_iter().chain(cpu)
    }

    /// When `job` first had waited long enough for either, if it has by `now`: a job that waited for CPU and now lacks
    /// memory, or the other way round, keeps what it waited.
    fn matured(&self, job: u64, now: SystemTime) -> Option<SystemTime> {
        let reached = |wait: Option<Wait>, limit: Duration| {
            wait.filter(|wait| wait.job == job)
                .map(|wait| wait.since + limit)
                .filter(|reached| *reached <= now)
        };
        let memory = reached(self.memory, MEMORY_PRIORITY_AFTER);
        let cpu = reached(self.cpu, CPU_PRIORITY_AFTER);
        memory.into_iter().chain(cpu).min()
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
    /// What the host's memory leaves the slice, by the margin in force now (ADR 0055).
    pub host: Option<i64>,
}

impl Drained {
    /// Whether a runner of `memory` and `cpu` fits, every known bound counted.
    fn holds(self, memory: u64, cpu: u64) -> bool {
        let fits = |room: Option<i64>, needs: u64| {
            room.is_none_or(|room| room >= i64::try_from(needs).unwrap_or(i64::MAX))
        };
        fits(self.memory, memory) && fits(self.host, memory) && fits(self.cpu, cpu)
    }
}

/// A job a plan held for something scarce, with a free slot for its runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Short {
    /// Its ID.
    pub job: u64,
    /// What it lacks first: memory before CPU, as `Waiting` says it.
    pub scarce: Scarce,
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
        drained.holds(short.memory, short.cpu).then_some(Barrier {
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
    /// How long from `now` it usually ends: at once if that time has passed. A clock that went back shortens what it
    /// ran, never lengthens what it has left past its usual run time.
    fn left_to_run(&self, now: SystemTime) -> Duration {
        let ran = now.duration_since(self.started_at).unwrap_or_default();
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
    /// Whether that profile has a free slot.
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

/// The room behind a barrier that cannot cost its owner what the bounds of ADR 0097 promise. With run times that hold,
/// everything started here has ended before the owner or any job older than it could start, so nothing they see changes
/// and the owner starts no later than with no backfill. Whatever the run times do, what started since the barrier
/// matured cannot take, even at its caps, what an empty slice has for the owner, nor the last free slot of its profile,
/// so the owner fits once the older jobs and what ran before have ended. Those are the only promises: a backfill may
/// well delay the owner when a job overruns.
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
            drained,
            runners,
            older,
        } = around;
        let mut ending: Vec<(Duration, Runner<'_>)> = runners
            .iter()
            .map(|runner| (runner.left_to_run(now), *runner))
            .collect();
        ending.sort_by_key(|(left_to_run, _)| *left_to_run);
        let ending = Ending(ending);
        let owner = ending.room_in(left, barrier.memory, barrier.cpu)?;
        let older = older.iter().filter_map(|job| ending.start_of(job, left));
        let until = older.fold(owner, Duration::min);
        if until.is_zero() {
            return None;
        }
        let mut spare = Left {
            memory: drained.memory,
            cpu: drained.cpu,
            host: drained.host,
        };
        spare.take(barrier.memory, barrier.cpu);
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
