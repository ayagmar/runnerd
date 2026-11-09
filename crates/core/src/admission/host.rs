//! What the host's memory leaves runners beside its other programs (ADR 0055): the slice's runners may hold what
//! they hold now without page cache plus the host's `MemAvailable`, less a margin kept for the host's page cache, so
//! reclaim stays cheap; and nothing starts while the host, short of memory, stalls for it. The stall bound has
//! hysteresis, and the margin grows by half only while no waiter fits the plain one (ADR 0102), so a host at the edge
//! does not start and hold runners by turns. A host whose memory can grow to a
//! ceiling, `[host] memory_ceiling` (ADR 0084), counts what it can still grow by as available and keeps the margin of
//! its ceiling; the stall rule reads the memory the host has now. Pure: the engine passes in the reading, with the ceiling,
//! and what it remembers of the last plan.

use super::pooled;
use crate::api::{HostBudget, HostCredit, HostMemory, host_margin};

/// Starts stop once every runnable task of the host waits for memory this share of the last 10 s, in hundredths of
/// a percent: 5%, while the host is short of memory (`stalls`).
pub const STALL_ENTER: u32 = 500;

/// Once stopped, starts resume when that share falls under this: 1%.
pub const STALL_LEAVE: u32 = 100;

/// Which margin the last plan left standing for the next. A restart begins at `Plain`, as nothing of it is stored; a
/// reload keeps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostBand {
    /// The plain margin.
    #[default]
    Plain,
    /// Jobs waited for the host's memory and none fitted even the plain margin (`short`): the margin grows by half.
    Larger,
    /// The host stalled: the margin grows by half, and starts resume only under `STALL_LEAVE`.
    Stalled,
}

impl HostBand {
    /// The band a plan leaves, by what `memory` leaves beside the `reserved` memory of what runs and the memory each
    /// job waiting for the host `needs`, and whether the host `stalled`: see `short`.
    pub fn after_plan(
        memory: &HostMemory,
        reserved: u64,
        stalled: bool,
        needs: impl IntoIterator<Item = u64>,
    ) -> Self {
        if stalled {
            HostBand::Stalled
        } else if short(memory, reserved, stalled, needs) {
            HostBand::Larger
        } else {
            HostBand::Plain
        }
    }

    /// Whether the plan left the larger margin, whatever runs now.
    pub fn is_short(self) -> bool {
        !matches!(self, HostBand::Plain)
    }

    /// Whether the larger margin stands for a slice in this state: the first runner into an empty slice needs only the
    /// margin.
    pub fn short(self, occupancy: Occupancy) -> bool {
        self.is_short() && occupancy == Occupancy::Occupied
    }

    /// Whether the host stalled at the plan that left this band.
    pub fn was_stalled(self) -> bool {
        self == HostBand::Stalled
    }
}

/// Whether any runner is in the slice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Occupancy {
    /// No runner: the first one needs only the margin.
    #[default]
    Empty,
    /// At least one runner.
    Occupied,
}

impl Occupancy {
    /// `Occupied` when any runner is in the slice.
    pub fn of(any_runner: bool) -> Self {
        if any_runner {
            Occupancy::Occupied
        } else {
            Occupancy::Empty
        }
    }
}

/// What admission goes by of the host: the band the last plan left, and the slice as it is now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Remembered {
    band: HostBand,
    occupancy: Occupancy,
}

impl Remembered {
    /// The `band` the last plan left, for a slice in this state.
    pub fn new(band: HostBand, occupancy: Occupancy) -> Self {
        Self { band, occupancy }
    }

    /// Whether the margin grows by half.
    pub fn short(self) -> bool {
        self.band.short(self.occupancy)
    }

    /// Whether the host stalled at the last plan: starts resume only under `STALL_LEAVE`.
    pub fn was_stalled(self) -> bool {
        self.band.was_stalled()
    }
}

#[cfg(test)]
impl Remembered {
    /// What a test starts from: whether the larger margin stands, and whether the host stalled. A stall without the
    /// larger margin is what a slice that emptied since reads.
    pub(crate) fn testing(short: bool, stalled: bool) -> Self {
        let occupancy = Occupancy::of(short);
        let band = match (short, stalled) {
            (_, true) => HostBand::Stalled,
            (true, false) => HostBand::Larger,
            (false, false) => HostBand::Plain,
        };
        Self { band, occupancy }
    }
}

/// The margin `MemAvailable` keeps beside the runners' predicted peaks for a host with `total` memory: `host_margin`,
/// half again as much while the host is `short`.
pub fn margin(total: u64, short: bool) -> u64 {
    let margin = host_margin(total);
    if short {
        margin.saturating_add(margin / 2)
    } else {
        margin
    }
}

/// Whether the larger margin stands for the next plan, by what `memory` leaves beside the `reserved` memory of what runs
/// and the memory each job waiting for the host `needs` (ADR 0102): while the host stalls, or while jobs wait and none
/// fits the plain margin. One that does fits at the next plan, so a queue does not hold the margin up by waiting in it.
/// A deficit stays negative here, so it never reads as a fit.
pub fn short(
    memory: &HostMemory,
    reserved: u64,
    stalled: bool,
    needs: impl IntoIterator<Item = u64>,
) -> bool {
    if stalled {
        return true;
    }
    let room = left(limit(memory, false), reserved, false);
    let mut needs = needs.into_iter().peekable();
    needs.peek().is_some()
        && needs.all(|needs| i64::try_from(needs).ok().is_none_or(|needs| needs > room))
}

/// What the host keeps past the margin before a credit stands, as a divisor of its memory: a quarter. A credit lets the
/// slice fill past what the plain count allows, so none stands near where sockets start to throttle, under about 20G of
/// 62G available, which the margin and this come to (ADR 0099).
const CREDIT_ALLOWANCE_SHARE: u64 = 4;

/// The host as the pooled count reads it, by the margin in force.
pub fn pooling(memory: &HostMemory, remembered: Remembered) -> pooled::Host {
    let stalled = stalls(memory, remembered.was_stalled());
    let margin = margin(memory.grown(), remembered.short() || stalled);
    pooled::Host {
        available: memory.available.saturating_add(memory.growable()),
        keeps: margin.saturating_add(memory.grown() / CREDIT_ALLOWANCE_SHARE),
        stalled,
    }
}

/// Whether `memory`'s stall shares say the host stalls for memory, given whether it did at the last plan. A stall
/// counts only while `MemAvailable` is under twice the margin: the shares are the whole host's, and a runner
/// reclaiming at its own cap raises them with memory to spare, which starts elsewhere do not make worse. A ceiling
/// changes nothing here (ADR 0084): what the host could still grow by is not available, and a stall while it is
/// short says the hypervisor is not giving it.
pub fn stalls(memory: &HostMemory, before: bool) -> bool {
    let full = memory.full.map_or(0, |full| full.avg10);
    let stalling = full >= STALL_ENTER || (before && full >= STALL_LEAVE);
    stalling && memory.available < margin(memory.total, false).saturating_mul(2)
}

/// What `memory` lets the slice's runners hold in all: what the slice holds now without page cache plus
/// `MemAvailable` and what the host can still grow by (`HostMemory::growable`), less the margin of the host's memory
/// at its ceiling, larger while the host is `short`. Without a ceiling above `MemTotal`, the slice and
/// `MemAvailable` less the margin of `MemTotal`.
pub fn limit(memory: &HostMemory, short: bool) -> u64 {
    memory
        .slice
        .saturating_add(memory.available)
        .saturating_add(memory.growable())
        .saturating_sub(margin(memory.grown(), short))
}

/// What `memory` leaves runners beside what runs in the slice, as the host's limit reads its `count`
/// (`pooled::Count::hosted`, ADR 0108), given what the last plan `remembered`.
pub fn budget(memory: HostMemory, count: pooled::Count, remembered: Remembered) -> HostBudget {
    let stalled = stalls(&memory, remembered.was_stalled());
    let limit = limit(&memory, remembered.short() || stalled);
    HostBudget {
        memory,
        limit,
        spare: u64::try_from(left(limit, count.hosted(), stalled)).unwrap_or(0),
        stalled,
        credit: (count.host_credit > 0).then_some(HostCredit {
            bytes: count.host_credit,
            used: count.used,
        }),
    }
}

/// What a host `limit` leaves new runners beside the `reserved` memory of what runs in the slice: negative by what
/// they reserve past it, none while the host is `stalled`. A runner that stops gives its reservation back to this,
/// so a deficit stays owed rather than hidden by `HostBudget::spare`, which stops at 0.
pub fn left(limit: u64, reserved: u64, stalled: bool) -> i64 {
    if stalled {
        return 0;
    }
    let bytes = |bytes: u64| i64::try_from(bytes).unwrap_or(i64::MAX);
    bytes(limit).saturating_sub(bytes(reserved))
}

/// What `memory` leaves a new runner beside the `reserved` memory of what runs or is reserved for before it starts,
/// such as the mirror: `budget`'s `spare` with no wait or stall remembered and none now. Beside only the mirror's
/// cap, the room of the first runner into an empty slice, which `runnerd init` sizes a starter runner within and
/// `runnerd doctor` checks profiles against.
pub fn first_runner(memory: &HostMemory, reserved: u64) -> u64 {
    u64::try_from(left(limit(memory, false), reserved, false)).unwrap_or(0)
}

#[cfg(test)]
mod tests;
