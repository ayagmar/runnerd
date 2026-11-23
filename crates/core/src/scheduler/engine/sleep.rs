//! Whether the host slept since the engine last looked, and what holds the engine back after it woke (ADR 0159). It
//! owns the last reading of the time suspended, the wake's settling and the runners that were up before the last
//! sleep. It reads no clock and calls nothing: the engine reads the clocks, and says what each wake undoes.

use std::collections::BTreeSet;

use crate::time::SLEPT_AFTER_MS;

/// Where the engine is after the last wake.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Woke {
    /// It never slept, or it has settled since: runners start as the budget allows.
    #[default]
    Settled,
    /// It woke at `at` (ms): no runner starts and no failure counts against a pool until a listing of queued jobs
    /// answered (`listed`) and, if the host was sampled before it slept (`sampled`), a host sample from after `at` is
    /// in. A host the sampler cannot read waits for no sample.
    Settling {
        at: u64,
        listed: bool,
        sampled: bool,
    },
}

/// What the engine knows of the host's sleeps. The three fields are independent: the reading is there from the first
/// look on, the settling is the last wake's, and the runners are those that were up before the last sleep, whether or
/// not the engine has settled since.
///
/// Nothing of it survives a restart but the runners that predate the last sleep, which a daemon that starts finds
/// again from when the history says it began (`predate`): it takes the time suspended as it finds it, and the time
/// before it is the store's downtime (ADR 0081). A reload keeps it.
#[derive(Debug, Default)]
pub(super) struct Sleep {
    /// The time suspended since boot at the last look, in ms (`Clock::suspended_ms`); `None` before the first.
    suspended: Option<u64>,
    woke: Woke,
    /// The runners that were up when the engine saw the last sleep: GitHub ended their sessions while the host slept,
    /// so none of them is a runner to keep waiting for a job.
    predating: BTreeSet<String>,
}

impl Sleep {
    /// Looks at the time suspended now, `suspended` ms since boot, at `now` (ms since the epoch, as readings' ages go)
    /// with `present` up and the host sampled before (`sampled`) or not. A rise of `SLEPT_AFTER_MS` or more since the
    /// last look is a sleep: the engine settles again from `now`, and `present` are the runners that predate it. Says
    /// how long it slept, in ms, if it did. The first look only takes the reading.
    pub(super) fn look(
        &mut self,
        suspended: u64,
        now: u64,
        present: impl IntoIterator<Item = String>,
        sampled: bool,
    ) -> Option<u64> {
        let last = self.suspended.replace(suspended)?;
        let slept = suspended.saturating_sub(last);
        if slept < SLEPT_AFTER_MS {
            return None;
        }
        self.woke = Woke::Settling {
            at: now,
            listed: false,
            sampled,
        };
        self.predating = present.into_iter().collect();
        Some(slept)
    }

    /// A listing of queued jobs that began at `began` (ms, as `look`'s `now`) answered. One that began before the wake
    /// may hold what GitHub listed before the sleep, so it does not settle anything.
    pub(super) fn listed(&mut self, began: u64) {
        if let Woke::Settling { at, listed, .. } = &mut self.woke
            && began >= *at
        {
            *listed = true;
        }
    }

    /// Whether the last wake still holds every start and keeps failures from counting against a pool, with the
    /// newest host sample taken at `sampled` (seconds since the epoch): until one from after the wake is in and a
    /// listing answered. A sample taken in the second of the wake may be from before it, so it does not count.
    pub(super) fn settling(&self, sampled: Option<u64>) -> bool {
        match self.woke {
            Woke::Settled => false,
            Woke::Settling {
                at,
                listed,
                sampled: awaited,
            } => {
                !listed
                    || awaited && sampled.is_none_or(|sampled| sampled.saturating_mul(1000) < at)
            }
        }
    }

    /// Settles, once `settling` says nothing holds the engine back any longer; whether it did now.
    pub(super) fn settle(&mut self, sampled: Option<u64>) -> bool {
        if matches!(self.woke, Woke::Settling { .. }) && !self.settling(sampled) {
            self.woke = Woke::Settled;
            return true;
        }
        false
    }

    /// `runners` were up before the last sleep began, as a daemon that starts finds them: none of them is a runner to
    /// keep waiting for a job either.
    pub(super) fn predate(&mut self, runners: impl IntoIterator<Item = String>) {
        self.predating.extend(runners);
    }

    /// Whether `runner` was up when the engine saw the last sleep.
    pub(super) fn predates(&self, runner: &str) -> bool {
        self.predating.contains(runner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rise_of_the_time_suspended_is_a_sleep_and_its_wake_settles_on_a_sample_and_a_listing() {
        let mut sleep = Sleep::default();
        assert_eq!(
            sleep.look(5_000, 1_000_000, [], true),
            None,
            "the first look only reads"
        );
        assert_eq!(
            sleep.look(5_000 + SLEPT_AFTER_MS - 1, 1_100_000, [], true),
            None
        );
        assert!(!sleep.settling(None), "a short pause holds nothing");
        let gap = sleep.look(
            5_000 + SLEPT_AFTER_MS - 1 + 3_600_000,
            5_000_500,
            ["r1".to_owned()],
            true,
        );
        assert_eq!(gap, Some(3_600_000));
        assert!(sleep.predates("r1"), "r1 was up before the sleep");
        assert!(!sleep.predates("r2"), "r2 was not");
        assert!(sleep.settling(Some(5_001)), "no listing answered yet");
        sleep.listed(5_000_499);
        assert!(
            sleep.settling(Some(5_001)),
            "a listing that began before the wake may show what GitHub listed before the sleep"
        );
        sleep.listed(5_000_500);
        assert!(
            sleep.settling(Some(5_000)),
            "a sample of the wake's second may be older"
        );
        assert!(sleep.settling(None), "no sample at all");
        assert!(!sleep.settle(Some(4_999)), "a sample from before the wake");
        assert!(sleep.settle(Some(5_001)), "settles on a later sample");
        assert!(!sleep.settling(None), "settled for good");
        assert!(sleep.predates("r1"), "r1 still predates the last sleep");
    }

    #[test]
    fn a_time_suspended_that_reads_lower_is_no_sleep() {
        let mut sleep = Sleep::default();
        sleep.look(9_000_000, 1_000, [], true);
        assert_eq!(sleep.look(0, 2_000, [], true), None);
        assert_eq!(sleep.look(SLEPT_AFTER_MS - 1, 3_000, [], true), None);
    }

    #[test]
    fn a_host_never_sampled_settles_on_a_listing_alone() {
        let mut sleep = Sleep::default();
        sleep.look(0, 1_000, [], false);
        assert!(sleep.look(SLEPT_AFTER_MS, 2_000_000, [], false).is_some());
        assert!(sleep.settling(None), "no listing answered yet");
        sleep.listed(u64::MAX);
        assert!(sleep.settle(None), "nothing else to wait for");
    }
}
