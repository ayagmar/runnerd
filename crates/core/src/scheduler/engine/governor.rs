//! How hard runnerd reads GitHub, by what is left of the rate limit (ADR 0127). It owns the readings of the last
//! ten minutes and the one pace step they put the reads at. It must not slow a registration or a delete, whatever the
//! limit says: only `reads_every`, `listings_every` and `active_listings_every` are read from outside, by the running
//! jobs' reads and the repositories' listings (ADR 0128), and nothing that sends a registration or a delete asks it anything. It must not parse a header or word a
//! count: `github::Github` reads the limit, and the log line is `Change::line`.
//!
//! Nothing survives a restart, and a reload keeps what it learned. A new engine starts at `Steady`, the pace of
//! before the governor, and learns the limit from the next answers: a step is judged only after `MIN_SPAN` of
//! readings, so at most that long at the old pace, in which the hour's budget is not spent. The limit used up, or a
//! secondary limit, still stops the requests as it did (ADR 0112); this only keeps the count from getting there.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use super::listing_pace::ACTIVE_SHARE;
use crate::api::duration;
use crate::github::Spend;

/// How far back the readings go.
const WINDOW: Duration = Duration::from_mins(10);
/// The least time of readings a step is judged on: a shorter span makes a burst of a few requests look like an hour's
/// pace.
const MIN_SPAN: Duration = Duration::from_mins(2);
/// The fewest requests kept for registrations and deletes: a burst of 16 jobs, twice over.
const MIN_RESERVE: u64 = 100;

/// How the reads are paced, from the pace before the governor to the slowest, one step at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(super) enum Step {
    /// `poll_interval` for everything.
    #[default]
    Steady,
    /// Running jobs every 2× `poll_interval`.
    JobReads2,
    /// Running jobs every 4× `poll_interval`.
    JobReads4,
    /// Active repositories' listings every `poll_interval`, as idle ones', not a third of it (ADR 0128).
    ActiveListings,
    /// Every listing every 2× `poll_interval`.
    Listings2,
    /// Every listing every 4× `poll_interval`.
    Listings4,
}

impl Step {
    const ORDER: [Step; 6] = [
        Step::Steady,
        Step::JobReads2,
        Step::JobReads4,
        Step::ActiveListings,
        Step::Listings2,
        Step::Listings4,
    ];

    fn up(self) -> Option<Step> {
        Step::ORDER.get(self as usize + 1).copied()
    }

    fn down(self) -> Option<Step> {
        Step::ORDER.get((self as usize).checked_sub(1)?).copied()
    }

    /// How many times more the reads cost once this step is undone, at most: what its slowing saved.
    fn undo_cost(self) -> u128 {
        match self {
            // Active listings at the share of `poll_interval` they go back to.
            Step::ActiveListings => u128::from(ACTIVE_SHARE),
            _ => 2,
        }
    }

    /// How many `poll_interval`s between reads of the running jobs.
    pub(super) fn reads(self) -> u32 {
        match self {
            Step::Steady => 1,
            Step::JobReads2 => 2,
            _ => 4,
        }
    }

    /// How many `poll_interval`s between listings.
    pub(super) fn listings(self) -> u32 {
        match self {
            Step::Listings2 => 2,
            Step::Listings4 => 4,
            _ => 1,
        }
    }
}

/// What the limit said, and what had been sent, when the engine looked.
#[derive(Debug)]
struct Reading {
    at: Instant,
    /// The pace in force when it was taken, which the window that judges a step starts from.
    step: Step,
    remaining: u64,
    /// When the count it belongs to starts over: a different one is another hour's.
    reset: SystemTime,
    /// Registrations and deletes sent so far.
    mutations: u64,
}

/// What the readings project until the limit resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Outlook {
    /// What the reads and listings, and anyone else sharing the token, will spend: the drop in `remaining` since the
    /// pace last changed, at most the last ten minutes, less the registrations and deletes, over the time left.
    pub(super) projected: u64,
    /// What they may spend: `remaining` less what the registrations and deletes will need.
    pub(super) room: u64,
}

impl Outlook {
    /// The step the projection calls for from `step`: one up if the spend exceeds the room, one down if it fits with
    /// 10% to spare even once what that step saved is spent again, so a step undone does not bring the one it undid.
    pub(super) fn verdict(self, step: Step) -> Step {
        if self.projected > self.room {
            return step.up().unwrap_or(step);
        }
        let projected = u128::from(self.projected) * step.undo_cost() * 10;
        if self.room > 0 && projected <= u128::from(self.room) * 9 {
            return step.down().unwrap_or(step);
        }
        step
    }
}

/// The projection from `readings`, oldest first, the newest taken `until_reset` before the limit starts over; `None`
/// before the pace has held for `MIN_SPAN`. Pure: it reads nothing but its arguments.
fn outlook(step: Step, readings: &VecDeque<Reading>, until_reset: Duration) -> Option<Outlook> {
    let (oldest, newest) = (readings.front()?, readings.back()?);
    // Since the pace last changed: a drop before it says nothing of what this pace costs.
    let since = readings
        .iter()
        .rev()
        .take_while(|r| r.step == step)
        .last()?;
    let span = newest.at.saturating_duration_since(since.at);
    if span < MIN_SPAN {
        return None;
    }
    let until = until_reset.as_millis();
    let dropped = u128::from(since.remaining.saturating_sub(newest.remaining));
    let own = u128::from(newest.mutations.saturating_sub(since.mutations));
    let projected = dropped.saturating_sub(own) * until / span.as_millis();
    // The registrations and deletes of the whole window, which the pace does not change.
    let sent = u128::from(newest.mutations.saturating_sub(oldest.mutations));
    let needed = sent * until / newest.at.saturating_duration_since(oldest.at).as_millis();
    let reserve = needed.max(u128::from(MIN_RESERVE));
    Some(Outlook {
        projected: u64::try_from(projected).unwrap_or(u64::MAX),
        room: u64::try_from(u128::from(newest.remaining).saturating_sub(reserve))
            .unwrap_or(u64::MAX),
    })
}

/// How long an active repository's listings are apart at `step`: `ACTIVE_SHARE` times per `poll_interval`, until the
/// step that holds them to an idle one's pace.
fn active_listings(step: Step, poll_interval: Duration) -> Duration {
    if step >= Step::ActiveListings {
        poll_interval * step.listings()
    } else {
        poll_interval / ACTIVE_SHARE
    }
}

/// Why the pace changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cause {
    /// What the readings projected, at `remaining` requests with `until_reset` to go.
    Projected {
        outlook: Outlook,
        remaining: u64,
        until_reset: Duration,
    },
    /// The limit's count started over.
    Reset,
}

/// A change of pace, for the one line that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Change {
    pub(super) from: Step,
    pub(super) to: Step,
    pub(super) cause: Cause,
}

impl Change {
    /// The log line, with `poll_interval` the pace is a multiple of.
    pub(super) fn line(&self, poll_interval: Duration) -> String {
        let pace = |step: Step| {
            format!(
                "running jobs every {}, listings every {} while active and {} while idle",
                duration((poll_interval * step.reads()).as_secs()),
                duration(active_listings(step, poll_interval).as_secs()),
                duration((poll_interval * step.listings()).as_secs()),
            )
        };
        let (way, why) = match self.cause {
            Cause::Projected {
                outlook,
                remaining,
                until_reset,
            } => (
                if self.to > self.from {
                    "slowed"
                } else {
                    "eased"
                },
                format!(
                    "{remaining} requests left, resets in {}, projected spend {} against room for {}",
                    duration(until_reset.as_secs()),
                    outlook.projected,
                    outlook.room
                ),
            ),
            Cause::Reset => ("eased", "the rate limit reset".to_owned()),
        };
        format!(
            "GitHub reads {way}: {} (was {}); {why}",
            pace(self.to),
            pace(self.from)
        )
    }
}

/// The readings of the last ten minutes, and the pace they put the reads at: the pace is the newest reading's.
#[derive(Debug, Default)]
pub(super) struct Governor {
    /// Oldest first, all of one count of the limit: the first of a new one clears them. The window reaches back to
    /// the last reading at or before ten minutes ago, so that ticks further apart than ten minutes still see the
    /// previous one.
    readings: VecDeque<Reading>,
}

impl Governor {
    /// Takes the reading `spend` gives at `now`, `wall` on the system's clock, and moves the pace one step if the
    /// projection calls for it. A limit not read, or already reset, changes nothing: the next answer carries one.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        wall: SystemTime,
        spend: Spend,
    ) -> Option<Change> {
        let limit = spend.rate_limit?;
        let until_reset = limit
            .reset
            .duration_since(wall)
            .ok()
            .filter(|left| !left.is_zero())?;
        let from = self.step();
        let started_over = self
            .readings
            .back()
            .is_some_and(|last| last.reset != limit.reset);
        if started_over {
            self.readings.clear();
        }
        self.readings.push_back(Reading {
            at: now,
            step: self.step(),
            remaining: limit.remaining,
            reset: limit.reset,
            mutations: spend.mutations,
        });
        if let Some(cutoff) = now.checked_sub(WINDOW) {
            while self.readings.get(1).is_some_and(|r| r.at <= cutoff) {
                self.readings.pop_front();
            }
        }
        let mut cause = started_over.then_some(Cause::Reset);
        let step = self.step();
        if let Some(outlook) = outlook(step, &self.readings, until_reset) {
            let next = outlook.verdict(step);
            if next != step {
                // The reading the new pace is judged from.
                if let Some(last) = self.readings.back_mut() {
                    last.step = next;
                }
                cause = Some(Cause::Projected {
                    outlook,
                    remaining: limit.remaining,
                    until_reset,
                });
            }
        }
        (self.step() != from).then_some(Change {
            from,
            to: self.step(),
            cause: cause?,
        })
    }

    /// How long the running jobs are read apart.
    pub(super) fn reads_every(&self, poll_interval: Duration) -> Duration {
        poll_interval * self.step().reads()
    }

    /// How long an idle repository's listings are apart.
    pub(super) fn listings_every(&self, poll_interval: Duration) -> Duration {
        poll_interval * self.step().listings()
    }

    /// How long an active repository's listings are apart (`active_listings`).
    pub(super) fn active_listings_every(&self, poll_interval: Duration) -> Duration {
        active_listings(self.step(), poll_interval)
    }

    /// The pace now: the newest reading's, `Steady` before any.
    pub(super) fn step(&self) -> Step {
        self.readings.back().map_or(Step::Steady, |r| r.step)
    }

    /// As if the newest reading, taken now, had moved the pace to `step`.
    #[cfg(test)]
    pub(super) fn testing_step(&mut self, step: Step) {
        self.readings.push_back(Reading {
            at: Instant::now(),
            step,
            remaining: 0,
            reset: SystemTime::UNIX_EPOCH,
            mutations: 0,
        });
    }

    /// What the readings project now, `until_reset` before the limit starts over.
    #[cfg(test)]
    pub(super) fn outlook(&self, until_reset: Duration) -> Option<Outlook> {
        outlook(self.step(), &self.readings, until_reset)
    }
}
