//! What the host's memory leaves runners beside its other programs (ADR 0055): the slice's runners may hold what
//! they hold now without page cache plus the host's `MemAvailable`, less a margin kept for the host's page cache, so
//! reclaim stays cheap; and nothing starts while the host, short of memory, stalls for it. Each bound has
//! hysteresis, so a host at the edge does not start and hold runners by turns. Pure: the engine passes in the
//! reading and what it remembers of the last plan.

use crate::api::{HostBudget, HostMemory};

/// Starts stop once every runnable task of the host waits for memory this share of the last 10 s, in hundredths of
/// a percent: 5%, while the host is short of memory (`stalls`).
pub const STALL_ENTER: u32 = 500;

/// Once stopped, starts resume when that share falls under this: 1%.
pub const STALL_LEAVE: u32 = 100;

/// The margin kept free at least: 2G.
const MARGIN_MIN: u64 = 2 << 30;

/// The margin as a share of the host's memory, as a divisor: a sixteenth, 4G of 64G.
const MARGIN_SHARE: u64 = 16;

/// What admission remembers of the host from the last plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Remembered {
    /// A job waited for the host's memory, or the host stalled: the margin grows by half until one fits again with
    /// it. The engine clears it while no runner runs, so the first runner into an empty slice needs only the margin.
    pub short: bool,
    /// The host stalled: starts resume only under `STALL_LEAVE`.
    pub stalled: bool,
}

/// The margin `MemAvailable` keeps beside the runners' predicted peaks for a host with `total` memory: the larger of
/// `MARGIN_MIN` and `total / MARGIN_SHARE`, half again as much while the host is `short`.
pub fn margin(total: u64, short: bool) -> u64 {
    let margin = MARGIN_MIN.max(total / MARGIN_SHARE);
    if short {
        margin.saturating_add(margin / 2)
    } else {
        margin
    }
}

/// Whether `memory`'s stall shares say the host stalls for memory, given whether it did at the last plan. A stall
/// counts only while `MemAvailable` is under twice the margin: the shares are the whole host's, and a runner
/// reclaiming at its own cap raises them with memory to spare, which starts elsewhere do not make worse.
pub fn stalls(memory: &HostMemory, before: bool) -> bool {
    let full = memory.full.map_or(0, |full| full.avg10);
    let stalling = full >= STALL_ENTER || (before && full >= STALL_LEAVE);
    stalling && memory.available < margin(memory.total, false).saturating_mul(2)
}

/// What `memory` lets the slice's runners hold in all: what the slice holds now without page cache plus
/// `MemAvailable`, less the margin, larger while the host is `short`.
pub fn limit(memory: &HostMemory, short: bool) -> u64 {
    memory
        .slice
        .saturating_add(memory.available)
        .saturating_sub(margin(memory.total, short))
}

/// What `memory` leaves runners beside the `reserved` memory of what runs in the slice, given what the last plan
/// `remembered`.
pub fn budget(memory: HostMemory, reserved: u64, remembered: Remembered) -> HostBudget {
    let stalled = stalls(&memory, remembered.stalled);
    let limit = limit(&memory, remembered.short || stalled);
    HostBudget {
        memory,
        limit,
        spare: u64::try_from(left(limit, reserved, stalled)).unwrap_or(0),
        stalled,
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
