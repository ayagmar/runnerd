//! One pool's health: what holds its runners back after a failure, and the two streaks that decide the next hold. It
//! owns that state and its rules, and must not call Docker, GitHub or the store: `runners` tells it what happened, and
//! logs and announces what it answers.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use super::runner_exit::Logged;
use super::runners::backoff;
use crate::api::HoldCause;
use crate::scheduler::Hold;

/// Runners in a row that connected and ended without a job, or waited `IDLE_TIMEOUT` for one (ADR 0104). Only none, one
/// and more than one differ. `beside` is the pool's other runners when the last of those ended: already up, they say
/// nothing of a later start, so each is let go once without counting.
#[derive(Debug, Default)]
enum IdleStreak {
    #[default]
    Quiet,
    Once {
        beside: BTreeSet<String>,
    },
    Repeated {
        beside: BTreeSet<String>,
    },
}

impl IdleStreak {
    /// Whether `runner` was already up when the last of the streak ended, which it says only once.
    fn lets_go(&mut self, runner: &str) -> bool {
        match self {
            Self::Quiet => false,
            Self::Once { beside } | Self::Repeated { beside } => beside.remove(runner),
        }
    }
}

/// A hold that took effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NewHold {
    /// How long no runner of the pool starts.
    pub(super) wait: Duration,
    /// Whether it is the first of its streak, the one announced.
    pub(super) first: bool,
}

/// A pool's runners held back after they failed, as `RepositoryState::holds` keeps them, and the streaks that decide
/// the next hold. The failure count and the idle streak are separate on purpose: a hold with no failure counted and an
/// idle streak beside a counted failure both occur.
///
/// Nothing of it survives a restart. A reload keeps it for a pool whose runners stay the same, and drops it for one
/// removed or changed: what failed may be what changed.
#[derive(Debug, Default)]
pub(super) struct PoolHealth {
    held: Option<Hold>,
    /// Holds in a row since a runner took a job or an idle streak started: how long the next lasts, and whether it is
    /// announced.
    failures: u32,
    idle: IdleStreak,
}

impl PoolHealth {
    /// One of the pool's runners ended, and this is what its log said. `beside` is the pool's other runners then.
    /// Answers what to hold the pool for, if anything: a runner that took a job clears everything, one that never
    /// connected is a failed start, and one that connected and left is a fault only the second time in a row (ADR
    /// 0104). A log Docker did not give says nothing, of the streaks or the hold.
    pub(super) fn runner_left(
        &mut self,
        logged: Logged,
        runner: &str,
        beside: BTreeSet<String>,
    ) -> Option<HoldCause> {
        match logged {
            Logged::TookJob => {
                *self = Self::default();
                None
            }
            Logged::NoJob { connected: true } => self.left_idle(runner, beside),
            Logged::NoJob { connected: false } => Some(HoldCause::NoJob),
            Logged::Unread => None,
        }
    }

    fn left_idle(&mut self, runner: &str, beside: BTreeSet<String>) -> Option<HoldCause> {
        if self.idle.lets_go(runner) {
            return None;
        }
        if matches!(self.idle, IdleStreak::Quiet) {
            // A failure before it is over, and the next one is a new streak's first, announced. Not so for the second
            // in a row, a fault of its own.
            self.failures = 0;
            self.idle = IdleStreak::Once { beside };
            return None;
        }
        self.idle = IdleStreak::Repeated { beside };
        Some(HoldCause::IdleExits)
    }

    /// Holds the pool back from `now`, longer each time in a row, unless it is held already: a runner that fails
    /// during a hold started before it, so says nothing of how long it should last. Answers the hold if one took effect.
    pub(super) fn hold(&mut self, cause: HoldCause, now: SystemTime) -> Option<NewHold> {
        if self.holding(now) {
            return None;
        }
        self.failures += 1;
        let wait = backoff(self.failures);
        self.held = Some(Hold {
            until: now + wait,
            cause,
        });
        Some(NewHold {
            wait,
            first: self.failures == 1,
        })
    }

    /// What holds the pool back, or last did: a plan compares it with its own `now`.
    pub(super) fn held(&self) -> Option<Hold> {
        self.held
    }

    /// Whether a failure holds the pool back at `now`.
    pub(super) fn holding(&self, now: SystemTime) -> bool {
        self.held.is_some_and(|held| held.until > now)
    }

    #[cfg(test)]
    pub(super) fn failures(&self) -> u32 {
        self.failures
    }

    /// The idle streak as a count: 0, 1 or 2 for more than one.
    #[cfg(test)]
    pub(super) fn idle_exits(&self) -> u32 {
        match self.idle {
            IdleStreak::Quiet => 0,
            IdleStreak::Once { .. } => 1,
            IdleStreak::Repeated { .. } => 2,
        }
    }

    /// Sets both streaks as a test names them, none of the idle one's runners beside.
    #[cfg(test)]
    pub(super) fn set_streaks(&mut self, failures: u32, idle_exits: u32) {
        let beside = BTreeSet::new();
        self.failures = failures;
        self.idle = match idle_exits {
            0 => IdleStreak::Quiet,
            1 => IdleStreak::Once { beside },
            _ => IdleStreak::Repeated { beside },
        };
    }

    #[cfg(test)]
    pub(super) fn set_held(&mut self, held: Option<Hold>) {
        self.held = held;
    }

    /// Ends a hold as of `past`, as if it had run out.
    #[cfg(test)]
    pub(super) fn expire(&mut self, past: SystemTime) {
        if let Some(held) = self.held.as_mut() {
            held.until = past;
        }
    }
}
