//! Which waits each queued job is in, and since when (ADR 0090, 0103, 0110, 0154): the one sizing reads, a slot's or
//! the host's memory's, and the cause the last plan held it for. A job has one entry, so it cannot be in two waits
//! sizing reads, nor held for two causes. What a wait adds up to is returned for the engine to record; this file reads
//! no clock and writes no store.

use std::collections::{BTreeMap, BTreeSet};

use crate::api::WaitCause;

/// What a job waits for, as sizing reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum WaitFor {
    /// A free slot of its profile, while the budgets have room for its runner (`Plan::slot_waiting`).
    Slot,
    /// The host's memory.
    Host,
}

/// One job's wait sizing reads.
#[derive(Debug, Clone, Copy)]
struct OpenWait {
    kind: WaitFor,
    /// The commit (ms) it was last counted up to.
    since: u64,
}

/// One job's wait for a cause.
#[derive(Debug, Clone, Copy)]
struct OpenCause {
    cause: WaitCause,
    /// The commit (ms) it was last counted up to.
    since: u64,
}

/// One job's open waits. Each is on its own: a job may wait for a slot sizing reads while its cause is a barrier, or
/// have a cause and no wait sizing reads.
#[derive(Debug, Clone, Copy, Default)]
struct JobWaits {
    sized: Option<OpenWait>,
    cause: Option<OpenCause>,
}

impl JobWaits {
    fn is_empty(&self) -> bool {
        self.sized.is_none() && self.cause.is_none()
    }

    fn take_sized(&mut self) -> Option<OpenWait> {
        self.sized.take()
    }

    fn set_sized(&mut self, wait: OpenWait) {
        self.sized = Some(wait);
    }

    fn take_cause(&mut self) -> Option<OpenCause> {
        self.cause.take()
    }

    fn set_cause(&mut self, cause: OpenCause) {
        self.cause = Some(cause);
    }
}

/// Time to add to the waits of `jobs`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Waited {
    pub kind: WaitFor,
    pub jobs: Vec<u64>,
    pub ms: u64,
}

/// Time to add to the waits of `jobs` for `cause`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CauseWaited {
    pub cause: WaitCause,
    pub jobs: Vec<u64>,
    pub ms: u64,
}

/// The waits the last plans left open, by job. Only its methods write it. Nothing survives a restart: a job a restart
/// left waiting counts again from the first plan after it, and what passed between is no cause's. A reload keeps every
/// wait; one of a repository the reload removed ends at the next commit, as that job is no longer planned.
#[derive(Debug, Default)]
pub(super) struct OpenWaits {
    open: BTreeMap<u64, JobWaits>,
}

impl OpenWaits {
    /// Commits a plan's waits of `kind`: `waiting` are in that wait from `now` on, a job of them in the other wait
    /// leaves it, and every wait of `kind` before this counts up to `now`. Says what to record, those that moved from
    /// the other wait first.
    pub(super) fn open_wait(
        &mut self,
        kind: WaitFor,
        waiting: BTreeSet<u64>,
        now: u64,
    ) -> Vec<Waited> {
        let mut waited = self.close(|job, open| open != kind && waiting.contains(&job), now);
        waited.extend(self.close(|_, open| open == kind, now));
        let since = OpenWait { kind, since: now };
        for job in waiting {
            self.open.entry(job).or_default().set_sized(since);
        }
        waited
    }

    /// Ends `job`'s wait sizing reads, if it has one, as its runner is in its slot: it waited until `now`, not until
    /// the next commit.
    pub(super) fn end_wait(&mut self, job: u64, now: u64) -> Option<Waited> {
        let wait = self.open.get_mut(&job)?.take_sized()?;
        self.forget_if_empty(job);
        Some(Waited {
            kind: wait.kind,
            jobs: vec![job],
            ms: now.saturating_sub(wait.since),
        })
    }

    /// The jobs in a wait of `kind`.
    pub(super) fn jobs(&self, kind: WaitFor) -> BTreeSet<u64> {
        self.open
            .iter()
            .filter(|(_, waits)| waits.sized.is_some_and(|wait| wait.kind == kind))
            .map(|(job, _)| *job)
            .collect()
    }

    /// Commits a plan's causes: each job of `held` waits for its cause from `now` on, a job of `going_on` (one the plan
    /// starts a runner for that has none yet) for the cause it had, and every other job's cause ends. Every cause
    /// before this counts up to `now`. Says what to record.
    pub(super) fn hold(
        &mut self,
        held: &BTreeMap<u64, WaitCause>,
        going_on: &BTreeSet<u64>,
        now: u64,
    ) -> Vec<CauseWaited> {
        let mut ended: BTreeMap<(WaitCause, u64), Vec<u64>> = BTreeMap::new();
        for (job, waits) in &mut self.open {
            let Some(open) = waits.take_cause() else {
                continue;
            };
            ended
                .entry((open.cause, open.since))
                .or_default()
                .push(*job);
            if !held.contains_key(job) && going_on.contains(job) {
                waits.set_cause(OpenCause { since: now, ..open });
            }
        }
        for (job, cause) in held {
            self.open.entry(*job).or_default().set_cause(OpenCause {
                cause: *cause,
                since: now,
            });
        }
        self.open.retain(|_, waits| !waits.is_empty());
        ended
            .into_iter()
            .map(|((cause, since), jobs)| CauseWaited {
                cause,
                jobs,
                ms: now.saturating_sub(since),
            })
            .collect()
    }

    /// A runner is in its slot for `job`: it waits for GitHub to hand the runner a job from `now` on, and its cause
    /// before counts up to `now`.
    pub(super) fn booting(&mut self, job: u64, now: u64) -> Option<CauseWaited> {
        let waits = self.open.entry(job).or_default();
        let ended = waits.take_cause();
        waits.set_cause(OpenCause {
            cause: WaitCause::Booting,
            since: now,
        });
        ended.map(|open| CauseWaited {
            cause: open.cause,
            jobs: vec![job],
            ms: now.saturating_sub(open.since),
        })
    }

    /// A runner took `job`: its cause counts up to `now` and ends.
    pub(super) fn taken(&mut self, job: u64, now: u64) -> Option<CauseWaited> {
        let open = self.open.get_mut(&job)?.take_cause()?;
        self.forget_if_empty(job);
        Some(CauseWaited {
            cause: open.cause,
            jobs: vec![job],
            ms: now.saturating_sub(open.since),
        })
    }

    /// The cause `job` is held for, if any.
    #[cfg(test)]
    pub(super) fn cause(&self, job: u64) -> Option<WaitCause> {
        self.open
            .get(&job)
            .and_then(|waits| waits.cause)
            .map(|open| open.cause)
    }

    fn forget_if_empty(&mut self, job: u64) {
        if self.open.get(&job).is_some_and(JobWaits::is_empty) {
            self.open.remove(&job);
        }
    }

    /// Ends the waits sizing reads that `ends` picks, up to `now`: one record for each kind and start they had.
    fn close(&mut self, ends: impl Fn(u64, WaitFor) -> bool, now: u64) -> Vec<Waited> {
        let mut ended: BTreeMap<(WaitFor, u64), Vec<u64>> = BTreeMap::new();
        for (job, waits) in &mut self.open {
            if waits.sized.is_some_and(|wait| ends(*job, wait.kind))
                && let Some(wait) = waits.take_sized()
            {
                ended.entry((wait.kind, wait.since)).or_default().push(*job);
            }
        }
        self.open.retain(|_, waits| !waits.is_empty());
        ended
            .into_iter()
            .map(|((kind, since), jobs)| Waited {
                kind,
                jobs,
                ms: now.saturating_sub(since),
            })
            .collect()
    }
}
