//! Which one pool goes first once memory or CPU keeps running out before its next job's runner fits (ADR 0096): how
//! long each pool's next job waited, what an empty slice would hold, and the rule that picks a plan's one barrier.
//! `plan` and `admission::simulate` both decide by it, so a replay shows what the daemon does. It does no I/O and
//! reads no clock; the engine keeps the waits, and it never stops or clamps what already runs.

use std::time::{Duration, SystemTime};

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

    /// Whether `job` has waited long enough for either at `now`: a job that waited for CPU and now lacks memory, or
    /// the other way round, keeps what it waited.
    fn starved(&self, job: u64, now: SystemTime) -> bool {
        let waited = |wait: Option<Wait>, limit: Duration| {
            wait.filter(|wait| wait.job == job)
                .and_then(|wait| now.duration_since(wait.since).ok())
                .is_some_and(|waited| waited >= limit)
        };
        waited(self.memory, MEMORY_PRIORITY_AFTER) || waited(self.cpu, CPU_PRIORITY_AFTER)
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
        (waits.starved(short.job, now) && drained.holds(short.memory, short.cpu)).then_some(
            Barrier {
                job: short.job,
                scarce: short.scarce,
            },
        )
    }

    /// Why a younger job waits behind it.
    pub fn reason(self) -> Waiting {
        match self.scarce {
            Scarce::Memory => Waiting::BehindMemory { job: self.job },
            Scarce::Cpu => Waiting::BehindCpu { job: self.job },
        }
    }
}
