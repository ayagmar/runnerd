//! What the host's memory leaves runners beside its other programs (ADR 0055): the slice's runners may hold what
//! they hold now without page cache plus the host's `MemAvailable`, less a margin kept for the host's page cache, so
//! reclaim stays cheap; and nothing starts while the host's tasks stall for memory. Each bound has hysteresis, so a
//! host at the edge does not start and hold runners by turns. Pure: the engine passes in the reading and what it
//! remembers of the last plan.

use crate::api::{HostBudget, HostMemory};

/// Starts stop once every runnable task of the host waits for memory this share of the last 10 s, in hundredths of
/// a percent: 5%.
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
    /// A job waited for the host's memory, or the host stalled: the margin is doubled until one fits again with it.
    pub short: bool,
    /// The host stalled: starts resume only under `STALL_LEAVE`.
    pub stalled: bool,
}

/// The margin `MemAvailable` keeps beside the runners' predicted peaks for a host with `total` memory: the larger of
/// `MARGIN_MIN` and `total / MARGIN_SHARE`, twice that while the host is `short`.
pub fn margin(total: u64, short: bool) -> u64 {
    let margin = MARGIN_MIN.max(total / MARGIN_SHARE);
    if short {
        margin.saturating_mul(2)
    } else {
        margin
    }
}

/// What `memory` leaves runners beside the `reserved` memory of what runs in the slice, given what the last plan
/// `remembered`.
pub fn budget(memory: HostMemory, reserved: u64, remembered: Remembered) -> HostBudget {
    let full = memory.full.map_or(0, |full| full.avg10);
    let stalled = full >= STALL_ENTER || (remembered.stalled && full >= STALL_LEAVE);
    let limit = memory
        .slice
        .saturating_add(memory.available)
        .saturating_sub(margin(memory.total, remembered.short || stalled));
    HostBudget {
        memory,
        limit,
        spare: if stalled {
            0
        } else {
            limit.saturating_sub(reserved)
        },
        stalled,
    }
}
