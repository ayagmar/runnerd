//! When one listed repository is listed next (ADR 0128): when its last listing began, and in how many of its probes
//! its runners are listed again. It owns that value and must not decide what makes a repository active or how long its interval is, nor list anything: `tick`
//! says which repositories a tick lists and tells it what each probe did.

use std::time::Duration;

use tokio::time::Instant;

/// An active repository is listed this many times every `poll_interval`, an idle one once (ADR 0128).
pub(super) const ACTIVE_SHARE: u32 = 3;

/// A repository's runners are listed on every this many probes of it, and on the next one after something it does
/// not see changed them.
const RUNNERS_EVERY: u8 = 3;

/// When one listed repository's next probe is, and whether that probe lists its runners. Its due time is not kept but
/// derived from the start of its last listing, so a reload's `poll_interval` and the governor's step apply at once.
///
/// Nothing of it survives a restart: a repository starts due, with its runners owed, as adopted runners count as busy
/// until a runners listing answers for them. A reload keeps it for a repository the config still lists; one it adds,
/// or adds back, starts as after a restart, and one it removes drops it with its state.
#[derive(Debug, Default)]
pub(super) struct ListingPace {
    /// When its last listing began, on Tokio's clock; `None` before its first.
    began: Option<Instant>,
    /// When its runners are listed next.
    runners: RunnersDue,
}

/// When a repository's runners are listed next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RunnersDue {
    /// At its next probe: none answered since its start, or one of its runners started or went since its last
    /// runners listing began, or that listing failed.
    #[default]
    Owed,
    /// After this many more probes that do not list them.
    After(u8),
}

/// Which runners listings a probe makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Runners {
    /// Every third probe's and those owed.
    ByTurn,
    /// This probe lists them, whatever their turn: a tick a command, a wake-up or a stop brings.
    Now,
}

impl ListingPace {
    /// When it is due with `every` between its listings: `None` when it never was listed, which is due now.
    pub(super) fn due(&self, every: Duration) -> Option<Instant> {
        self.began.map(|began| began + every)
    }

    /// Whether it is due at `now` with `every` between its listings.
    pub(super) fn is_due(&self, now: Instant, every: Duration) -> bool {
        self.due(every).is_none_or(|due| due <= now)
    }

    /// A probe of it begins at `at`; says whether it lists its runners, which `runners` decides or their turn.
    pub(super) fn begin(&mut self, at: Instant, runners: Runners) -> bool {
        self.began = Some(at);
        let lists = match (runners, self.runners) {
            (Runners::Now, _) | (Runners::ByTurn, RunnersDue::Owed | RunnersDue::After(0)) => true,
            (Runners::ByTurn, RunnersDue::After(_)) => false,
        };
        self.runners = match self.runners {
            _ if lists => RunnersDue::After(RUNNERS_EVERY - 1),
            RunnersDue::After(left) => RunnersDue::After(left.saturating_sub(1)),
            RunnersDue::Owed => RunnersDue::Owed,
        };
        lists
    }

    /// One of its runners started or went, or its runners listing failed: its next probe lists them.
    pub(super) fn runners_owed(&mut self) {
        self.runners = RunnersDue::Owed;
    }
}

#[cfg(test)]
impl ListingPace {
    /// As if its last listing began `by` earlier.
    pub(super) fn age(&mut self, by: Duration) {
        self.began = self.began.and_then(|began| began.checked_sub(by));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY: Duration = Duration::from_secs(5);

    #[tokio::test(start_paused = true)]
    async fn a_repository_never_listed_is_due_and_lists_its_runners() {
        let mut pace = ListingPace::default();
        let now = Instant::now();
        assert!(pace.is_due(now, EVERY), "never listed is due");
        assert!(
            pace.begin(now, Runners::ByTurn),
            "the first probe lists runners"
        );
        assert!(!pace.is_due(now, EVERY), "just listed");
        assert!(
            pace.is_due(now + EVERY, EVERY),
            "due an interval after it began"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn runners_are_listed_every_third_probe_unless_owed_or_asked() {
        let mut pace = ListingPace::default();
        let now = Instant::now();
        let probes: Vec<bool> = (0..7).map(|_| pace.begin(now, Runners::ByTurn)).collect();
        assert_eq!(probes, [true, false, false, true, false, false, true]);

        pace.begin(now, Runners::ByTurn);
        pace.runners_owed();
        assert!(
            pace.begin(now, Runners::ByTurn),
            "owed after a start lists them"
        );
        assert!(
            !pace.begin(now, Runners::ByTurn),
            "and counts again from there"
        );
        assert!(pace.begin(now, Runners::Now), "a command's tick lists them");
        assert!(
            !pace.begin(now, Runners::ByTurn),
            "which counts as their turn"
        );
    }
}
