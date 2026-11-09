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

/// The last hold of a pool, with how many came in a row. It stays once it has run out, so that the next failure goes on
/// counting, until a runner takes a job.
#[derive(Debug, Clone, Copy)]
struct Backoff {
    hold: Hold,
    /// Holds in a row since a runner took a job or an idle streak started: how long the next lasts, and whether it is
    /// announced.
    failures: u32,
}

/// A pool's runners held back after they failed, as `RepositoryState::holds` keeps them, and the streaks that decide
/// the next hold. The failure count and the idle streak are separate on purpose: a hold with no failure counted and an
/// idle streak beside a counted failure both occur.
///
/// Nothing of it survives a restart. A reload keeps it for a pool whose runners stay the same, and drops it for one
/// removed or changed: what failed may be what changed.
#[derive(Debug, Default)]
pub(super) struct PoolHealth {
    held: Option<Backoff>,
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
            if let Some(held) = self.held.as_mut() {
                held.failures = 0;
            }
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
        let failures = self.failures() + 1;
        let wait = backoff(failures);
        self.held = Some(Backoff {
            hold: Hold {
                until: now + wait,
                cause,
            },
            failures,
        });
        Some(NewHold {
            wait,
            first: failures == 1,
        })
    }

    /// What holds the pool back, or last did: a plan compares it with its own `now`.
    pub(super) fn held(&self) -> Option<Hold> {
        self.held.map(|held| held.hold)
    }

    /// Whether a failure holds the pool back at `now`.
    pub(super) fn holding(&self, now: SystemTime) -> bool {
        self.held.is_some_and(|held| held.hold.until > now)
    }

    /// Holds in a row, as `Backoff::failures` counts them.
    pub(super) fn failures(&self) -> u32 {
        self.held.map_or(0, |held| held.failures)
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

    /// Sets both streaks as a test names them, none of the idle one's runners beside. Failures counted with no hold
    /// come with one long over, as they do in a running daemon.
    #[cfg(test)]
    pub(super) fn set_streaks(&mut self, failures: u32, idle_exits: u32) {
        let beside = BTreeSet::new();
        self.held = match (self.held(), failures) {
            (None, 0) => None,
            (hold, failures) => Some(Backoff {
                hold: hold.unwrap_or(Hold {
                    until: SystemTime::UNIX_EPOCH,
                    cause: HoldCause::Start,
                }),
                failures,
            }),
        };
        self.idle = match idle_exits {
            0 => IdleStreak::Quiet,
            1 => IdleStreak::Once { beside },
            _ => IdleStreak::Repeated { beside },
        };
    }

    /// Sets the hold, keeping the count of failures.
    #[cfg(test)]
    pub(super) fn set_held(&mut self, hold: Hold) {
        self.held = Some(Backoff {
            hold,
            failures: self.failures(),
        });
    }

    /// Ends a hold as of `past`, as if it had run out.
    #[cfg(test)]
    pub(super) fn expire(&mut self, past: SystemTime) {
        if let Some(held) = self.held.as_mut() {
            held.hold.until = past;
        }
    }
}
