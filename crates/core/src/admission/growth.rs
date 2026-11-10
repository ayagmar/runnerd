//! The growth brake the density study proposes beside the growth count (`pooled::Snapshot::largest`): every plan keeps a
//! look at what each runner uses and the room the count kept (`pooled::growth_room`), and when the runners present at
//! a look up to `GROWTH_WINDOW` old grew, together, past the room that look kept, the count is plain for `BRAKE_FOR`.
//! The count keeps room for a few runners' growth; this is what answers growth beyond them, such as a cold cache across
//! many jobs at once. Only `simulate --brake` reads it yet. It owns its looks and its state; it reads no clock and
//! does no I/O, and it stops nothing that runs.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, SystemTime};

/// How far back a look may be for the growth since it to engage the brake: 10 minutes.
pub const GROWTH_WINDOW: Duration = Duration::from_mins(10);

/// How long the count stays plain once the brake engages: 30 minutes from the last look that engaged it.
pub const BRAKE_FOR: Duration = Duration::from_mins(30);

/// What one runner uses at a look: its fresh sample, 0 before its first. A runner whose samples say nothing has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use<Id> {
    /// The runner.
    pub runner: Id,
    /// Bytes.
    pub memory: u64,
}

/// Why the count is plain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Braked {
    /// What ran grew `grew` bytes since a look, past the `kept` bytes of room that look kept, until `until`.
    Grew {
        /// When the count may go back to the growth count.
        until: SystemTime,
        /// How much the runners present at both looks grew, together.
        grew: u64,
        /// The room the earlier look kept.
        kept: u64,
    },
    /// The daemon started at most `GROWTH_WINDOW` ago and has no window to judge growth by yet; looks are not stored,
    /// and runners it adopted started where it did not see them.
    Restarted {
        /// When the window is full.
        until: SystemTime,
    },
}

impl Braked {
    /// Until when it stands.
    pub fn until(self) -> SystemTime {
        match self {
            Braked::Grew { until, .. } | Braked::Restarted { until } => until,
        }
    }
}

/// One plan's look.
#[derive(Debug, Clone)]
struct Look<Id> {
    at: SystemTime,
    /// By runner, what it used.
    used: BTreeMap<Id, u64>,
    /// The room the count kept then.
    room: u64,
}

/// The looks of the last `GROWTH_WINDOW` and the brake they set. Nothing of it is stored: a daemon's restart begins at
/// `restarted`, a replay at `default`; a reload keeps it.
#[derive(Debug, Clone)]
pub struct Growth<Id> {
    looks: VecDeque<Look<Id>>,
    braked: Option<Braked>,
}

impl<Id> Default for Growth<Id> {
    /// No look yet, and no brake.
    fn default() -> Self {
        Self {
            looks: VecDeque::new(),
            braked: None,
        }
    }
}

impl<Id: Ord> Growth<Id> {
    /// What a daemon starts from at `now`: plain until it has a window of looks to judge growth by.
    pub fn restarted(now: SystemTime) -> Self {
        Self {
            looks: VecDeque::new(),
            braked: Some(Braked::Restarted {
                until: now + GROWTH_WINDOW,
            }),
        }
    }

    /// Takes a plan's look at `now`: what each runner `used` and the `room` the count keeps for growth. If the runners
    /// present at an earlier look in the window grew, together, past the room that look kept, the brake engages until
    /// `BRAKE_FOR` from now; a brake already past ends.
    pub fn look(&mut self, now: SystemTime, used: impl IntoIterator<Item = Use<Id>>, room: u64) {
        let used: BTreeMap<Id, u64> = used
            .into_iter()
            .map(|runner| (runner.runner, runner.memory))
            .collect();
        let from = now
            .checked_sub(GROWTH_WINDOW)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        while self.looks.front().is_some_and(|look| look.at < from) {
            self.looks.pop_front();
        }
        let grown = self
            .looks
            .iter()
            .map(|look| (grew(&look.used, &used), look.room))
            .filter(|(grew, room)| grew > room)
            .max_by_key(|(grew, room)| grew - room);
        if let Some((grew, kept)) = grown {
            self.braked = Some(Braked::Grew {
                until: now + BRAKE_FOR,
                grew,
                kept,
            });
        } else if self.braked.is_some_and(|braked| braked.until() <= now) {
            self.braked = None;
        }
        self.looks.push_back(Look {
            at: now,
            used,
            room,
        });
    }

    /// The brake in force at `now`, if any: the count is then plain.
    pub fn braked(&self, now: SystemTime) -> Option<Braked> {
        self.braked.filter(|braked| now < braked.until())
    }
}

/// How much the runners in both `then` and `now` grew between them, together; 0 if they shrank.
fn grew<Id: Ord>(then: &BTreeMap<Id, u64>, now: &BTreeMap<Id, u64>) -> u64 {
    let (before, after) = then
        .iter()
        .filter_map(|(runner, before)| Some((*before, *now.get(runner)?)))
        .fold((0_u64, 0_u64), |(before, after), (was, is)| {
            (before.saturating_add(was), after.saturating_add(is))
        });
    after.saturating_sub(before)
}
