//! How long each pool's next job has waited for memory or CPU, and whether a barrier has served the pool (ADR 0096, 0098,
//! 0115). `Waits` is one value that only its own methods here write, in the engine and in a replay alike; what the waits
//! decide is in `priority`.

use std::time::{Duration, SystemTime};

use super::priority::{PRIORITY_AFTER, SERVED_PRIORITY_AFTER, Scarce};

/// Since when a job has waited for something scarce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    /// The job's ID: a wait counts for this job alone, so it cannot outlive it.
    pub job: u64,
    /// When a scheduling plan first held it.
    pub since: SystemTime,
}

/// What a pool's next job has waited for, and whether a barrier has served the pool. Each clock runs from the first plan
/// that held the job for it and goes on through a plan that holds it for the other, or for a slot; the two are separate
/// because a pool's next job for memory and its next job for CPU can be different jobs.
///
/// Only these methods write it, here or in a replay (ADR 0115). Nothing of it survives a restart; a reload keeps the
/// waits of the pools still configured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Waits {
    /// Its wait for memory.
    memory: Option<Wait>,
    /// Its wait for CPU.
    cpu: Option<Wait>,
    /// The waiting job, if a plan's barrier was ever its own. Not only the last plan's: the plan that starts it has no
    /// barrier, as it fits, and a start refused, or that failed preparing its caches, plans again. A failure that brings
    /// a backoff clears this with the waits.
    owner: Option<u64>,
    /// Whether a runner of the pool started for a job that owned a barrier, since its queue last emptied: its next jobs
    /// wait `SERVED_PRIORITY_AFTER`.
    served: bool,
    /// Whether the last plan held the pool's next job for CPU, as the status says. A start or a listing between two
    /// plans leaves it: only the next plan says otherwise.
    cpu_held: bool,
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
    /// served the pool; whether this start is the one that served it. A proposed start, a deferred one or one that
    /// failed comes to nothing, so it does not count.
    pub fn started(&mut self) -> bool {
        let served = self.served || self.owner.is_some();
        let newly = served && !self.served;
        *self = Waits {
            served,
            cpu_held: self.cpu_held,
            ..Waits::default()
        };
        newly
    }

    /// The pool may no longer start a runner: nothing is kept for it.
    pub fn stopped(&mut self) {
        *self = Waits::default();
    }

    /// Says whether the plan just made held the pool's next job for CPU.
    pub fn set_cpu_held(&mut self, held: bool) {
        self.cpu_held = held;
    }

    /// Whether the last plan held the pool's next job for CPU.
    pub fn cpu_held(&self) -> bool {
        self.cpu_held
    }

    /// Its wait for CPU, if any.
    pub fn cpu(&self) -> Option<Wait> {
        self.cpu
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
    pub(super) fn matured(&self, job: u64, now: SystemTime) -> Option<SystemTime> {
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

    /// Its wait for memory, for tests to read.
    pub(crate) fn memory(&self) -> Option<Wait> {
        self.memory
    }

    /// The job that owned a barrier, for tests to read.
    pub(crate) fn owner(&self) -> Option<u64> {
        self.owner
    }

    /// Whether a barrier has served the pool, for tests to read.
    pub(crate) fn served(&self) -> bool {
        self.served
    }

    /// Its wait for memory, for tests that age it.
    pub(crate) fn memory_mut(&mut self) -> Option<&mut Wait> {
        self.memory.as_mut()
    }

    /// Its wait for CPU, for tests that age it.
    pub(crate) fn cpu_mut(&mut self) -> Option<&mut Wait> {
        self.cpu.as_mut()
    }
}
