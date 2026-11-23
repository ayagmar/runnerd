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
    /// When (ms) it was last counted up to: the cause's start, or the last checkpoint since.
    since: u64,
}

/// How long a cause that goes on stays uncounted before the next plan counts it, in ms: the first plan this long after
/// the last checkpoint counts every open cause up to it. No timer of its own runs it, so a crash loses up to this plus
/// the time to that plan, `[host] poll_interval` and the tick's own time. A cause that changes or ends is counted at
/// once (ADR 0154).
pub(super) const CAUSE_CHECKPOINT: u64 = 5 * 60 * 1000;

/// Where one job's cause is.
#[derive(Debug, Clone, Copy, Default)]
enum Cause {
    /// No plan holds it for one.
    #[default]
    None,
    /// The last plan held it for this cause, or its runner is starting.
    Open(OpenCause),
    /// A runner took it since the last commit: a plan made before the take may still hold it, and must not open a
    /// wait for it again. The next commit forgets it.
    Taken,
}

/// One job's open waits. Each is on its own: a job may wait for a slot sizing reads while its cause is a barrier, or
/// have a cause and no wait sizing reads.
#[derive(Debug, Clone, Copy, Default)]
struct JobWaits {
    sized: Option<OpenWait>,
    cause: Cause,
}

impl JobWaits {
    fn is_empty(&self) -> bool {
        self.sized.is_none() && matches!(self.cause, Cause::None)
    }

    fn open_cause(&self) -> Option<OpenCause> {
        match self.cause {
            Cause::Open(open) => Some(open),
            Cause::None | Cause::Taken => None,
        }
    }

    fn is_taken(&self) -> bool {
        matches!(self.cause, Cause::Taken)
    }

    fn take_sized(&mut self) -> Option<OpenWait> {
        self.sized.take()
    }

    fn set_sized(&mut self, wait: OpenWait) {
        self.sized = Some(wait);
    }

    /// Ends the open cause, if any; a taken job stays taken.
    fn take_cause(&mut self) -> Option<OpenCause> {
        let open = self.open_cause()?;
        self.cause = Cause::None;
        Some(open)
    }

    fn set_cause(&mut self, cause: OpenCause) {
        self.cause = Cause::Open(cause);
    }

    fn set_taken(&mut self) {
        self.cause = Cause::Taken;
    }

    /// The next commit forgets a take: every plan from then on was made after it.
    fn forget_taken(&mut self) {
        if self.is_taken() {
            self.cause = Cause::None;
        }
    }
}

/// Time to add to the waits of `jobs`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Waited {
    pub kind: WaitFor,
    pub jobs: Vec<u64>,
    pub ms: u64,
}

/// Time to add to `job`'s wait for `cause`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CauseWaited {
    pub job: u64,
    pub cause: WaitCause,
    pub ms: u64,
}

/// The waits the last plans left open, by job, and when the open causes were last counted. Only its methods write it.
/// Nothing survives a restart: a job a restart left waiting counts again from the first plan after it, and what passed
/// between is no cause's, as is a cause's time since it was last counted, `CAUSE_CHECKPOINT` plus the time to the next
/// plan at most; a stop asked for, whichever its mode, ends every cause once it has read the takes the captures
/// reported, so a stop that is not a crash loses none. A reload keeps every wait; one of a repository the reload removed
/// ends at the next commit, as that job is no longer planned.
#[derive(Debug, Default)]
pub(super) struct OpenWaits {
    open: BTreeMap<u64, JobWaits>,
    /// When (ms) every open cause was last counted up to.
    checkpointed: u64,
}

impl OpenWaits {
    /// Commits a plan's waits of `kind`: `waiting` are in that wait from `now` on, except a job a runner took since the
    /// last commit of causes, a job of them in the other wait leaves it, and every wait of `kind` before this counts up
    /// to `now`. Says what to record, those that moved from
    /// the other wait first.
    pub(super) fn open_wait(
        &mut self,
        kind: WaitFor,
        waiting: BTreeSet<u64>,
        now: u64,
    ) -> Vec<Waited> {
        let waiting: BTreeSet<u64> = waiting
            .into_iter()
            .filter(|job| !self.open.get(job).is_some_and(JobWaits::is_taken))
            .collect();
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
    /// starts a runner for that has none yet) for the cause it had, and every other job's cause ends. A cause that ends
    /// or changes counts up to `now`; one that goes on is not counted, unless `CAUSE_CHECKPOINT` has passed since the
    /// last checkpoint, when every open cause counts up to `now`. A job a runner took since the last commit opens no
    /// cause, as the plan may have been made before the take; the commit forgets the take. Says what to record.
    pub(super) fn hold(
        &mut self,
        held: &BTreeMap<u64, WaitCause>,
        going_on: &BTreeSet<u64>,
        now: u64,
    ) -> Vec<CauseWaited> {
        let checkpoint = now.saturating_sub(self.checkpointed) >= CAUSE_CHECKPOINT;
        let mut waited = Vec::new();
        for (job, waits) in &mut self.open {
            let Some(open) = waits.open_cause() else {
                continue;
            };
            let next = held
                .get(job)
                .copied()
                .or_else(|| going_on.contains(job).then_some(open.cause));
            if next == Some(open.cause) && !checkpoint {
                continue;
            }
            if let Some(open) = waits.take_cause() {
                waited.push(CauseWaited {
                    job: *job,
                    cause: open.cause,
                    ms: now.saturating_sub(open.since),
                });
            }
            if let Some(cause) = next {
                waits.set_cause(OpenCause { cause, since: now });
            }
        }
        for (job, cause) in held {
            let waits = self.open.entry(*job).or_default();
            if matches!(waits.cause, Cause::None) {
                waits.set_cause(OpenCause {
                    cause: *cause,
                    since: now,
                });
            }
        }
        for waits in self.open.values_mut() {
            waits.forget_taken();
        }
        self.open.retain(|_, waits| !waits.is_empty());
        if checkpoint {
            self.checkpointed = now;
        }
        waited
    }

    /// A runner is in its slot for `job`: it waits for GitHub to hand the runner a job from `now` on, and its cause
    /// before counts up to `now`. A job a runner took already waits no more.
    pub(super) fn booting(&mut self, job: u64, now: u64) -> Option<CauseWaited> {
        let waits = self.open.entry(job).or_default();
        if waits.is_taken() {
            return None;
        }
        let ended = waits.take_cause();
        waits.set_cause(OpenCause {
            cause: WaitCause::Booting,
            since: now,
        });
        ended.map(|open| CauseWaited {
            job,
            cause: open.cause,
            ms: now.saturating_sub(open.since),
        })
    }

    /// `job`'s cause ends at `at`, as its start failed: it counts up to `at`, or to its own start if that was later,
    /// and the next plan may hold it again.
    pub(super) fn end_cause(&mut self, job: u64, at: u64) -> Option<CauseWaited> {
        let open = self.open.get_mut(&job)?.take_cause()?;
        self.forget_if_empty(job);
        Some(CauseWaited {
            job,
            cause: open.cause,
            ms: at.saturating_sub(open.since),
        })
    }

    /// A runner took `job` at `at`: its cause counts up to `at`, or to its own start if that was later, and no plan made
    /// before the next commit opens one again, nor a wait sizing reads.
    pub(super) fn taken(&mut self, job: u64, at: u64) -> Option<CauseWaited> {
        let waits = self.open.entry(job).or_default();
        let open = waits.take_cause();
        waits.set_taken();
        open.map(|open| CauseWaited {
            job,
            cause: open.cause,
            ms: at.saturating_sub(open.since),
        })
    }

    /// A stop: every open cause counts up to `now` and ends, as no plan holds a job until the next start.
    pub(super) fn end_causes(&mut self, now: u64) -> Vec<CauseWaited> {
        let mut waited = Vec::new();
        for (job, waits) in &mut self.open {
            waits.forget_taken();
            if let Some(open) = waits.take_cause() {
                waited.push(CauseWaited {
                    job: *job,
                    cause: open.cause,
                    ms: now.saturating_sub(open.since),
                });
            }
        }
        self.open.retain(|_, waits| !waits.is_empty());
        waited
    }

    /// The host slept from `from` (ms) on (ADR 0159): every wait and cause ends there, as nothing waited for a slot,
    /// the host or a cause while it slept, and the next plan opens them again from its own time. A take stays until
    /// the next commit. Says what to record.
    pub(super) fn slept(&mut self, from: u64) -> (Vec<Waited>, Vec<CauseWaited>) {
        let waited = self.close(|_, _| true, from);
        let mut caused = Vec::new();
        for (job, waits) in &mut self.open {
            if let Some(open) = waits.take_cause() {
                caused.push(CauseWaited {
                    job: *job,
                    cause: open.cause,
                    ms: from.saturating_sub(open.since),
                });
            }
        }
        self.open.retain(|_, waits| !waits.is_empty());
        self.checkpointed = self.checkpointed.max(from);
        (waited, caused)
    }

    /// The next `hold` is a checkpoint, as it would be `CAUSE_CHECKPOINT` on.
    #[cfg(test)]
    pub(super) fn checkpoint_due(&mut self) {
        self.checkpointed = 0;
    }

    /// The cause `job` is held for, if any.
    #[cfg(test)]
    pub(super) fn cause(&self, job: u64) -> Option<WaitCause> {
        self.open
            .get(&job)
            .and_then(JobWaits::open_cause)
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
