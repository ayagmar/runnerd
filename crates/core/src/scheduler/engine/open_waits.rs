//! Which wait each queued job is in, a slot's or the host's memory's, and since when (ADR 0090, 0103, 0110). A job has
//! one entry, so it cannot be in both. What a wait adds up to is returned for the engine to record; this file reads no
//! clock and writes no store.

use std::collections::{BTreeMap, BTreeSet};

/// What a job waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum WaitFor {
    /// A free slot of its profile, while the budgets have room for its runner (`Plan::slot_waiting`).
    Slot,
    /// The host's memory.
    Host,
}

/// One job's wait.
#[derive(Debug, Clone, Copy)]
struct OpenWait {
    kind: WaitFor,
    /// The commit (ms) it was last counted up to.
    since: u64,
}

/// Time to add to the waits of `jobs`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Waited {
    pub kind: WaitFor,
    pub jobs: Vec<u64>,
    pub ms: u64,
}

/// The waits the last plans left open, one per job. Only `open_wait` and `end_wait` write it. Nothing survives a
/// restart: a job a restart left waiting counts again from the first plan after it. A reload keeps every wait; one of a
/// repository the reload removed ends at the next commit of its kind, as that job is no longer planned.
#[derive(Debug, Default)]
pub(super) struct OpenWaits {
    open: BTreeMap<u64, OpenWait>,
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
        self.open
            .extend(waiting.into_iter().map(|job| (job, since)));
        waited
    }

    /// Ends `job`'s wait, if it has one, as its runner is in its slot: it waited until `now`, not until the next commit.
    pub(super) fn end_wait(&mut self, job: u64, now: u64) -> Option<Waited> {
        self.open.remove(&job).map(|wait| Waited {
            kind: wait.kind,
            jobs: vec![job],
            ms: now.saturating_sub(wait.since),
        })
    }

    /// The jobs in a wait of `kind`.
    pub(super) fn jobs(&self, kind: WaitFor) -> BTreeSet<u64> {
        self.open
            .iter()
            .filter(|(_, wait)| wait.kind == kind)
            .map(|(job, _)| *job)
            .collect()
    }

    /// Ends the waits `ends` picks, up to `now`: one record for each kind and start they had.
    fn close(&mut self, ends: impl Fn(u64, WaitFor) -> bool, now: u64) -> Vec<Waited> {
        let mut ended: BTreeMap<(WaitFor, u64), Vec<u64>> = BTreeMap::new();
        self.open.retain(|&job, wait| {
            let end = ends(job, wait.kind);
            if end {
                ended.entry((wait.kind, wait.since)).or_default().push(job);
            }
            !end
        });
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
