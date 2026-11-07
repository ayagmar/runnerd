//! The pooled count (ADR 0099): one aggregate figure for what the slice's runners take of the budget, a bounded credit
//! below adding every runner's worst case. Jobs rarely peak at once, so the room between what each eligible runner
//! counts and what it uses adds by root-sum-square across jobs, and linearly within a job, whose copies grow together.
//! The host's limit takes its own credit off the same plain count (ADR 0108): it counts what eligible runners use and
//! keeps room for the two largest growths they may still make, at least `HOST_RESERVE_FLOOR`. Nothing here reaches a
//! runner: its reservation, what it learns from and what is stored of it stay as they are. `count` is the one
//! implementation: the engine and `admission::simulate` build a `Snapshot` and call it. Pure: no I/O, no clock.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{HIGH_WATER, Key};
use crate::api::Basis;

/// A job's first minute counts in full: its setup says nothing of what it will use.
pub const POOL_AFTER: Duration = Duration::from_secs(60);

/// The most the credit takes off the count: 6G. Past that the replay buys little and spends the high-water margin
/// (ADR 0099).
pub const MOST_CREDIT: u64 = 6 << 30;

/// What the credit leaves under the high-water mark beside what the slice holds now: 2G.
pub const HIGH_WATER_RESERVE: u64 = 2 << 30;

/// How many jobs' growth the host's limit keeps room for: the two with the most headroom left. Realised growth past
/// that was 0.04% of moments over the 65 hours the host study read, none of them past what the plain count missed too
/// (ADR 0108).
pub const HOST_RESERVE_JOBS: usize = 2;

/// The least room the host's limit keeps for growth: 6G, what `MOST_CREDIT` lets `MemoryMax` spend (ADR 0108). With
/// one or two eligible jobs whose headroom adds to less, the host's limit reads the plain count.
pub const HOST_RESERVE_FLOOR: u64 = 6 << 30;

/// What a runner holds of the count.
#[derive(Debug, Clone, Copy)]
pub struct Runner<'run> {
    /// Its job, once known.
    pub key: Option<&'run Key>,
    /// What it takes of the budget, added up as it always was: at least what it uses.
    pub takes: u64,
    /// What it uses, from a recent sample without page cache; `None`, it says nothing of that.
    pub used: Option<u64>,
    /// How long its job has run, for a job this daemon saw start; `None` for one it did not, as after a restart.
    pub ran: Option<Duration>,
    /// What its reservation goes by; `None` without a record of its admission.
    pub basis: Option<Basis>,
}

impl Runner<'_> {
    /// What it uses, if the credit may count on the room above that. A runner with a job not known yet may be given a
    /// heavy one, a job just started has not shown what it uses, and a reservation at its cap or past an overrun says
    /// its job does not keep to its prediction.
    fn pooled(&self) -> Option<u64> {
        let predicted = matches!(
            self.basis,
            Some(Basis::Job { .. } | Basis::Profile { .. } | Basis::ProfileMax { .. })
        );
        let settled = self.ran.is_some_and(|ran| ran >= POOL_AFTER);
        self.used
            .filter(|_| predicted && settled && self.key.is_some())
    }
}

/// What the slice holds, from a recent sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    /// Its memory as the high-water mark reads it, page cache read twice included.
    pub memory: u64,
    /// Its `MemoryMax`; `None` where the slice has none, which leaves `MemoryMax`'s credit no room. The host's credit
    /// needs only the sample.
    pub memory_max: Option<u64>,
}

/// What the host has, from a recent reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    /// What it has available, with what it can still grow by.
    pub available: u64,
    /// What must stay available after the credit is spent: the margin in force and `host::allowance`.
    pub keeps: u64,
    /// Whether its tasks stall for memory.
    pub stalled: bool,
}

/// What the aggregate count is read from: the same at every moment it is read, by the daemon or a replay.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'run> {
    /// The runners in slots.
    pub runners: &'run [Runner<'run>],
    /// What no runner holds: seed containers, shells and the mirror.
    pub committed: u64,
    /// The slice; `None` without a recent sample, when no credit is safe.
    pub slice: Option<Slice>,
    /// The host; `None` without a recent reading, when no credit is safe.
    pub host: Option<Host>,
    /// The most the credit may be: `MOST_CREDIT`; 0 with pooling off and under caps admission.
    pub most: u64,
    /// Whether the host's limit may take its credit (ADR 0108): measured admission with `[host] pooled_host`, apart
    /// from `most`.
    pub host_reserve: bool,
}

/// The aggregate count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    /// Every runner's take added up, with what no runner holds.
    pub plain: u64,
    /// What pooling takes off it.
    pub credit: u64,
    /// How many runners the credit counts on.
    pub runners: usize,
    /// What the host's limit takes off `plain` (ADR 0108): the eligible jobs' headroom past what it keeps for growth.
    pub host_credit: u64,
    /// What the runners use, by their recent samples: the part of `hosted` that is no room for growth.
    pub used: u64,
    /// How much more `host_credit` could be with no runner starting: what `Backfill` expects of it.
    pub host_credit_room: u64,
}

impl Count {
    /// What `MemoryMax` reads: `plain` less `credit`.
    pub fn pooled(self) -> u64 {
        self.plain - self.credit
    }

    /// What the host's limit reads: `plain` less `host_credit`.
    pub fn hosted(self) -> u64 {
        self.plain - self.host_credit
    }
}

/// The count of `snapshot`:
///
/// ```text
/// P = every runner's take + what no runner holds
/// Q = the ineligible part + eligible use + sqrt(sum over jobs of H²), H = a job's takes less its use
/// credit = min(P - Q, most, 90% of MemoryMax - slice - 2G, available - what the host keeps)
/// count = P - credit
/// ```
///
/// The host's limit reads its own count:
///
/// ```text
/// H1, H2      = the two largest H
/// host credit = sum H - max(6G, H1 + H2), at least 0          (none without host_reserve, a recent slice sample and
///                                                               host reading, or while the host stalls)
/// hosted      = P - host credit = the ineligible part + eligible use + min(sum H, max(6G, H1 + H2))
/// ```
///
/// A runner a plan starts is never eligible, so it raises `P` and `Q` alike, and both credits stay: a plan's starts
/// take from what is left byte for byte. So does a warm runner's stop.
pub fn count(snapshot: &Snapshot<'_>) -> Count {
    let takes: u64 = snapshot.runners.iter().map(|runner| runner.takes).sum();
    let plain = takes.saturating_add(snapshot.committed);
    let mut room: BTreeMap<&Key, u64> = BTreeMap::new();
    let mut runners = 0;
    for runner in snapshot.runners {
        if let (Some(key), Some(used)) = (runner.key, runner.pooled()) {
            *room.entry(key).or_default() += runner.takes.saturating_sub(used);
            runners += 1;
        }
    }
    let added: u64 = room.values().sum();
    let squares: u128 = room
        .values()
        .map(|room| u128::from(*room) * u128::from(*room))
        .sum();
    let credit = added
        .saturating_sub(root(squares))
        .min(snapshot.most)
        .min(snapshot.slice.map_or(0, slice_room))
        .min(snapshot.host.map_or(0, host_room));
    let read = snapshot.slice.is_some() && snapshot.host.is_some_and(|host| !host.stalled);
    let host_credit = if snapshot.host_reserve && read {
        added.saturating_sub(kept_for_growth(room.into_values().collect()))
    } else {
        0
    };
    let used = snapshot
        .runners
        .iter()
        .map(|runner| runner.used.unwrap_or(0).min(runner.takes))
        .sum();
    Count {
        plain,
        credit,
        runners: if credit > 0 { runners } else { 0 },
        host_credit,
        used,
        // A job's headroom is at most its takes, so the host's credit is at most every take less the floor, whatever
        // the runners use later: a runner that shrinks raises it with no end.
        host_credit_room: if snapshot.host_reserve {
            takes
                .saturating_sub(HOST_RESERVE_FLOOR)
                .saturating_sub(host_credit)
        } else {
            0
        },
    }
}

/// What the host's limit keeps of the jobs' headroom `room` for growth: the `HOST_RESERVE_JOBS` largest added up,
/// at least `HOST_RESERVE_FLOOR`.
fn kept_for_growth(mut room: Vec<u64>) -> u64 {
    room.sort_unstable_by(|a, b| b.cmp(a));
    room.iter()
        .take(HOST_RESERVE_JOBS)
        .sum::<u64>()
        .max(HOST_RESERVE_FLOOR)
}

/// What the credit may be beside `slice`: its room under the high-water mark, less `HIGH_WATER_RESERVE`.
fn slice_room(slice: Slice) -> u64 {
    let Some(memory_max) = slice.memory_max else {
        return 0;
    };
    let mark = u128::from(memory_max) * u128::from(HIGH_WATER) / 100;
    u64::try_from(mark)
        .unwrap_or(u64::MAX)
        .saturating_sub(slice.memory)
        .saturating_sub(HIGH_WATER_RESERVE)
}

/// What the credit may be beside `host`: what it has available past what it keeps; none while it stalls.
fn host_room(host: Host) -> u64 {
    if host.stalled {
        return 0;
    }
    host.available.saturating_sub(host.keeps)
}

/// The square root of `squares`, rounded up: a credit errs low.
fn root(squares: u128) -> u64 {
    let root = squares.isqrt();
    let exact = root * root == squares;
    u64::try_from(root + u128::from(!exact)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
