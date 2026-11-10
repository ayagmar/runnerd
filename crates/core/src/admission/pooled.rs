//! The pooled count (ADR 0099): one aggregate figure for what the slice's runners take of the budget, a bounded credit
//! below adding every runner's worst case. Jobs rarely peak at once, so the room between what each eligible runner
//! counts and what it uses adds by root-sum-square across jobs, and linearly within a job, whose copies grow together.
//! The host's limit takes its own credit off the same plain count (ADR 0108): it counts what eligible runners use and
//! keeps room for the two largest growths they may still make, at least `HOST_RESERVE_FLOOR`. Nothing here reaches a
//! runner: its reservation, what it learns from and what is stored of it stay as they are. `count` is the one
//! implementation: the engine and `admission::simulate` build a `Snapshot` and call it. Pure: no I/O, no clock.
//!
//! It also has the growth count (ADR 0130), which both budgets read in place of those credits while it holds: what every
//! runner uses, with room for its three largest growths and every young runner's, both credits bounded as 0099 bounds
//! `MemoryMax`'s; below the guard's line, or while its brake (`growth::Growth`) holds, the credits above. `simulate
//! --count` replays it and the variants it was chosen from.

use std::collections::BTreeMap;
use std::time::Duration;

use super::trace::Reading;
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

/// How many jobs' growth the growth count keeps room for: the three with the most headroom. Counting young and unnamed
/// runners too, the recorded runner sets' real 10-minute growth passed three such headrooms 0.01% of the time and two
/// 0.08% (density study).
pub const GROWTH_JOBS: usize = 3;

/// The least room the growth count keeps for growth: 6G, as `HOST_RESERVE_FLOOR`.
pub const GROWTH_FLOOR: u64 = 6 << 30;

/// The growth count the daemon counts by (ADR 0130): room for the three largest growths and every young runner's,
/// within 0099's bounds, and the credits below the guard's line.
pub const GROWTH: Growth = Growth {
    jobs: GROWTH_JOBS,
    bound: Bound::HighWater,
    young: Young::Covered,
    below: Below::Credits,
    hold: Hold::Free,
};

/// How the growth count counts (ADR 0130): one value for the daemon, `GROWTH`, and the variants `simulate` compares
/// it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Growth {
    /// How many runners' growth it keeps room for, the largest.
    pub jobs: usize,
    /// What bounds the credit it gives.
    pub bound: Bound,
    /// Whether the room it keeps covers every young runner's growth.
    pub young: Young,
    /// What counts below the guard's line, and while the brake holds.
    pub below: Below,
    /// Whether the brake holds the growth count back now.
    pub hold: Hold,
}

impl Growth {
    /// The count of the density study, as PR 1's replay had it: room for the `jobs` largest growths, unbounded, the
    /// plain count below the guard's line.
    pub fn top(jobs: usize) -> Self {
        Self {
            jobs,
            bound: Bound::Open,
            young: Young::Ranked,
            below: Below::Plain,
            hold: Hold::Free,
        }
    }

    /// This count with its brake holding it back.
    #[must_use]
    pub fn held(self) -> Self {
        Self {
            hold: Hold::Held,
            ..self
        }
    }
}

/// What bounds the growth count's credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// 0099's bounds: `MemoryMax`'s credit at most the slice's room under the high-water mark less 2G, and both at most
    /// what the host has available past what it keeps.
    HighWater,
    /// Only the room kept for growth.
    Open,
}

/// Whether the room the growth count keeps covers every young runner's growth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Young {
    /// It keeps at least the headroom of every runner whose job this daemon has not seen run `POOL_AFTER` yet: what a
    /// burst of starts may grow by together is counted, not only the largest few.
    Covered,
    /// A young runner's headroom is ranked with the others'.
    Ranked,
}

/// What counts where the growth count does not: below the guard's line and while the brake holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Below {
    /// 0099's and 0108's credits, as `Snapshot::most` and `Snapshot::host_reserve` allow.
    Credits,
    /// The plain count.
    Plain,
}

/// Whether the brake (`growth::Growth`) holds the growth count back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// It counts, above the guard's line.
    Free,
    /// What `Growth::below` says counts.
    Held,
}

/// What a runner holds of the count.
#[derive(Debug, Clone, Copy)]
pub struct Runner<'run> {
    /// Its job, once known.
    pub key: Option<&'run Key>,
    /// What it takes of the budget, added up as it always was: at least what it uses.
    pub takes: u64,
    /// What it uses, from a recent sample without page cache: `Reading::NotYet` for a runner just started, whose
    /// first sample is still to come; `Reading::Stale` for one whose samples say nothing of that, as a sampler gap or
    /// samples with page cache.
    pub used: Reading,
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
        self.sampled()
            .filter(|_| predicted && settled && self.key.is_some())
    }

    /// Whether the growth count keeps room for all of its growth (`Young::Covered`): its first sample is still to
    /// come, or this daemon has not seen its job run `POOL_AFTER`, as a runner waiting for a job, one it adopted, or
    /// one just started.
    fn young(&self) -> bool {
        matches!(self.used, Reading::NotYet) || self.ran.is_none_or(|ran| ran < POOL_AFTER)
    }

    /// What its fresh sample says it uses, at most what it takes.
    fn sampled(&self) -> Option<u64> {
        match self.used {
            Reading::Used(used) => Some(used.min(self.takes)),
            Reading::NotYet | Reading::Stale => None,
        }
    }

    /// What it may still grow by, by the growth count: what it takes past its use; all of it before its first sample.
    /// `None` for a runner whose samples say nothing, which counts its whole take.
    fn headroom(&self) -> Option<u64> {
        match self.used {
            Reading::Used(used) => Some(self.takes.saturating_sub(used)),
            Reading::NotYet => Some(self.takes),
            Reading::Stale => None,
        }
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
    /// Whether the host's limit may take its credit (ADR 0108), apart from `most`.
    pub host_reserve: bool,
    /// With `Some`, the growth count instead of both credits (ADR 0130), which go by `most` and `host_reserve` only
    /// where it says (`Growth::below`).
    pub growth: Option<Growth>,
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
    /// How much more `credit` could be with no runner starting: what `Backfill` expects of it. While the growth
    /// count may count, as much as it could take off once every runner used nothing, braked or not.
    pub credit_room: u64,
    /// How much more `host_credit` could be with no runner starting, as `credit_room`.
    pub host_credit_room: u64,
    /// What the growth count kept for growth, when it counted; `None` under the credits, below the guard's line and
    /// while its brake holds.
    pub kept: Option<u64>,
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
/// take from what is left byte for byte. So does a warm runner's stop. Not under the growth count (`Snapshot::growth`):
/// a start or a stop moves it by what it changes of the room kept, so a plan counts again after each (`Tally`).
pub fn count(snapshot: &Snapshot<'_>) -> Count {
    let takes: u64 = snapshot.runners.iter().map(|runner| runner.takes).sum();
    let plain = takes.saturating_add(snapshot.committed);
    match snapshot.growth {
        Some(growth) => grown(snapshot, plain, growth),
        None => credits(snapshot, plain),
    }
}

/// The count of `snapshot` by 0099's and 0108's credits, whose plain count is `plain`.
fn credits(snapshot: &Snapshot<'_>, plain: u64) -> Count {
    let takes = plain.saturating_sub(snapshot.committed);
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
    let used = used(snapshot.runners);
    Count {
        plain,
        credit,
        runners: if credit > 0 { runners } else { 0 },
        host_credit,
        used,
        credit_room: snapshot.most.saturating_sub(credit),
        // A job's headroom is at most its takes, so the host's credit is at most every take less the floor, whatever
        // the runners use later: a runner that shrinks raises it with no end.
        host_credit_room: if snapshot.host_reserve {
            takes
                .saturating_sub(HOST_RESERVE_FLOOR)
                .saturating_sub(host_credit)
        } else {
            0
        },
        kept: None,
    }
}

/// What `runners` use by their fresh samples, each at most what it takes.
fn used(runners: &[Runner<'_>]) -> u64 {
    runners.iter().filter_map(Runner::sampled).sum()
}

/// The growth count of `snapshot`, whose plain count is `plain`, as `growth` counts:
///
/// ```text
/// use_i   = runner i's fresh sample; g_i = its take less that, its whole take before its first sample
/// stale   = runners whose samples say nothing, at their take
/// room    = max(floor, the `jobs` largest g_i, with Young::Covered the sum of every young runner's g_i)
/// count   = committed + sum use_i + sum stale + min(sum g_i, room)
/// credit  = plain - count; with Bound::HighWater, MemoryMax's at most `slice_room` and both at most `host_room`
/// ```
///
/// Below the guard's line, and while the brake holds it, `growth.below` counts: without a recent slice sample or host
/// reading, while the host stalls, or while what the host has available is under what it keeps, the margin in force
/// and a quarter of its memory, 0099's line. A start adds its whole take as headroom: it raises the count only past
/// what the room already kept, by all of it while the young runners' growth sets the room.
fn grown(snapshot: &Snapshot<'_>, plain: u64, growth: Growth) -> Count {
    let used = used(snapshot.runners);
    // With no runner starting, the count is lowest once every runner uses nothing: its take is then all headroom.
    let takes: Vec<u64> = snapshot.runners.iter().map(|runner| runner.takes).collect();
    let lowest = snapshot
        .committed
        .saturating_add(takes.iter().sum::<u64>().min(room(&takes, growth.jobs)));
    let low = snapshot
        .host
        .is_none_or(|host| host.stalled || host.available < host.keeps);
    if growth.hold == Hold::Held || snapshot.slice.is_none() || low {
        let below = match growth.below {
            Below::Credits => credits(snapshot, plain),
            Below::Plain => Count {
                plain,
                used,
                ..Count::default()
            },
        };
        // Held, it may fall by the credits' own room, or as far as the growth count's lowest once it counts again.
        return Count {
            credit_room: below.credit_room.max(below.pooled().saturating_sub(lowest)),
            host_credit_room: below
                .host_credit_room
                .max(below.hosted().saturating_sub(lowest)),
            kept: None,
            ..below
        };
    }
    let stale: u64 = snapshot
        .runners
        .iter()
        .filter(|runner| runner.headroom().is_none())
        .map(|runner| runner.takes)
        .sum();
    let headroom: u64 = snapshot.runners.iter().filter_map(Runner::headroom).sum();
    let kept = headroom.min(growth_room(snapshot.runners, growth));
    let counted = snapshot
        .committed
        .saturating_add(used)
        .saturating_add(stale)
        .saturating_add(kept);
    let credit = plain.saturating_sub(counted);
    let (credit, host_credit) = match growth.bound {
        Bound::HighWater => {
            let host = snapshot.host.map_or(0, host_room);
            let slice = snapshot.slice.map_or(0, slice_room);
            (credit.min(slice).min(host), credit.min(host))
        }
        Bound::Open => (credit, credit),
    };
    let count = Count {
        plain,
        credit,
        runners: snapshot
            .runners
            .iter()
            .filter(|runner| runner.headroom().is_some())
            .count(),
        host_credit,
        used,
        credit_room: 0,
        host_credit_room: 0,
        kept: Some(kept),
    };
    Count {
        credit_room: count.pooled().saturating_sub(lowest),
        host_credit_room: count.hosted().saturating_sub(lowest),
        ..count
    }
}

/// A `Snapshot` a plan owns and counts again after each start and stop (ADR 0130), each runner known by `Id`, such as
/// its slot. Under 0099's and 0108's credits a start or a stop moves the count by its take; under the growth count by
/// what it changes of the room kept, which only counting again tells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally<Id> {
    runners: Vec<Tallied<Id>>,
    committed: u64,
    slice: Option<Slice>,
    host: Option<Host>,
    most: u64,
    host_reserve: bool,
    growth: Option<Growth>,
}

/// One runner of a `Tally`, as `Runner` owned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tallied<Id> {
    /// `None` for a start the plan made.
    id: Option<Id>,
    key: Option<Key>,
    takes: u64,
    used: Reading,
    ran: Option<Duration>,
    basis: Option<Basis>,
}

impl<Id: PartialEq> Tally<Id> {
    /// What `snapshot` counts, its runners known by `ids`, in their order.
    pub fn new(snapshot: &Snapshot<'_>, ids: impl IntoIterator<Item = Id>) -> Self {
        Self {
            runners: snapshot
                .runners
                .iter()
                .zip(ids)
                .map(|(runner, id)| Tallied {
                    id: Some(id),
                    key: runner.key.cloned(),
                    takes: runner.takes,
                    used: runner.used,
                    ran: runner.ran,
                    basis: runner.basis,
                })
                .collect(),
            committed: snapshot.committed,
            slice: snapshot.slice,
            host: snapshot.host,
            most: snapshot.most,
            host_reserve: snapshot.host_reserve,
            growth: snapshot.growth,
        }
    }

    /// The count now.
    pub fn count(&self) -> Count {
        let runners: Vec<Runner<'_>> = self
            .runners
            .iter()
            .map(|runner| Runner {
                key: runner.key.as_ref(),
                takes: runner.takes,
                used: runner.used,
                ran: runner.ran,
                basis: runner.basis,
            })
            .collect();
        count(&Snapshot {
            runners: &runners,
            committed: self.committed,
            slice: self.slice,
            host: self.host,
            most: self.most,
            host_reserve: self.host_reserve,
            growth: self.growth,
        })
    }

    /// A runner starts taking `takes`: one just started, with no job and no sample yet.
    pub fn start(&mut self, takes: u64) {
        self.runners.push(Tallied {
            id: None,
            key: None,
            takes,
            used: Reading::NotYet,
            ran: Some(Duration::ZERO),
            basis: None,
        });
    }

    /// The runner `id` stops.
    pub fn stop(&mut self, id: &Id) {
        self.runners.retain(|runner| runner.id.as_ref() != Some(id));
    }
}

/// The room the growth count keeps for `runners`' growth, as `growth` keeps it: their `jobs` largest headrooms added
/// up, at least `GROWTH_FLOOR`, and with `Young::Covered` at least every young runner's headroom. Their growth together
/// past this is what `growth::Growth` brakes on.
pub fn growth_room(runners: &[Runner<'_>], growth: Growth) -> u64 {
    let headroom: Vec<u64> = runners.iter().filter_map(Runner::headroom).collect();
    let largest = room(&headroom, growth.jobs);
    match growth.young {
        Young::Covered => largest.max(
            runners
                .iter()
                .filter(|runner| runner.young())
                .filter_map(Runner::headroom)
                .sum(),
        ),
        Young::Ranked => largest,
    }
}

/// The `jobs` largest of `headroom` added up, at least `GROWTH_FLOOR`.
fn room(headroom: &[u64], jobs: usize) -> u64 {
    largest(headroom.to_vec(), jobs).max(GROWTH_FLOOR)
}

/// What the host's limit keeps of the jobs' headroom `room` for growth: the `HOST_RESERVE_JOBS` largest added up,
/// at least `HOST_RESERVE_FLOOR`.
fn kept_for_growth(room: Vec<u64>) -> u64 {
    largest(room, HOST_RESERVE_JOBS).max(HOST_RESERVE_FLOOR)
}

/// The `jobs` largest of `values` added up.
fn largest(mut values: Vec<u64>, jobs: usize) -> u64 {
    values.sort_unstable_by(|a, b| b.cmp(a));
    values.iter().take(jobs).sum()
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
