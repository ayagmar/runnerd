//! The host's bound on a host whose memory is fixed, and on one that can grow to `[host] memory_ceiling`
//! (ADR 0084): what the slice may hold, the margin it keeps and the stall rule, which a ceiling leaves alone.

use super::{Remembered, budget, first_runner, limit, margin, stalls};
use crate::api::{HostMemory, Pressure};

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;

/// A reading of a host with `total` memory, `available` of it, the slice holding nothing, no stall and no ceiling.
fn host(total: u64, available: u64) -> HostMemory {
    HostMemory {
        at: 0,
        total,
        available,
        slice: 0,
        some: Some(Pressure::default()),
        full: Some(Pressure::default()),
        holders: Vec::new(),
        ceiling: None,
    }
}

/// The idle VM of issue 159: Hyper-V shrank it to 2.12G, 1.48G of it available, and it can grow to 24G.
fn idle_vm(ceiling: Option<u64>) -> HostMemory {
    HostMemory {
        ceiling,
        ..host(2 * G + 120 * M, G + 490 * M)
    }
}

/// `memory` with every runnable task stalled `avg10` hundredths of a percent of the last 10 s.
fn stalled(memory: HostMemory, avg10: u32) -> HostMemory {
    HostMemory {
        full: Some(Pressure { avg10, avg60: 0 }),
        ..memory
    }
}

#[test]
fn without_a_ceiling_the_slice_may_hold_what_is_available_less_the_margin_of_mem_total() {
    let memory = HostMemory {
        slice: 9 * G,
        ..host(64 * G, 31 * G)
    };
    assert_eq!(margin(64 * G, false), 4 * G);
    assert_eq!(limit(&memory, false), 9 * G + 31 * G - 4 * G);
    assert_eq!(limit(&memory, true), 9 * G + 31 * G - 6 * G);
    assert_eq!(first_runner(&memory, 9 * G), 27 * G);
}

#[test]
fn an_idle_vm_without_a_ceiling_has_nothing_to_spare() {
    assert_eq!(
        first_runner(&idle_vm(None), 0),
        0,
        "1.48G is under the 2G margin"
    );
}

#[test]
fn a_ceiling_counts_what_the_host_can_still_grow_by_and_keeps_the_ceilings_margin() {
    let memory = idle_vm(Some(24 * G));
    assert_eq!(memory.growable(), 24 * G - (2 * G + 120 * M));
    // 1.48G available and 21.88G to grow by, less the 2G margin of 24G: room for the 8G runner the VM waited for.
    let expected = G + 490 * M + (24 * G - (2 * G + 120 * M)) - 2 * G;
    assert_eq!(limit(&memory, false), expected);
    assert_eq!(first_runner(&memory, 0), expected);
    assert!(first_runner(&memory, 0) >= 8 * G, "an 8G runner fits");
}

#[test]
fn the_margin_follows_the_ceiling_not_mem_total() {
    // A 16G VM that can grow to 96G keeps the 6G margin of 96G, not the 2G of 16G.
    let memory = HostMemory {
        ceiling: Some(96 * G),
        ..host(16 * G, 10 * G)
    };
    assert_eq!(limit(&memory, false), 10 * G + 80 * G - 6 * G);
    assert_eq!(limit(&memory, true), 10 * G + 80 * G - 9 * G);
}

#[test]
fn a_ceiling_at_or_under_mem_total_changes_nothing() {
    let fixed = HostMemory {
        slice: 9 * G,
        ..host(64 * G, 31 * G)
    };
    for ceiling in [G, 32 * G, 64 * G] {
        let memory = HostMemory {
            ceiling: Some(ceiling),
            ..fixed.clone()
        };
        assert_eq!(memory.growable(), 0, "{ceiling}");
        for short in [false, true] {
            assert_eq!(limit(&memory, short), limit(&fixed, short), "{ceiling}");
        }
        let with = budget(memory, 4 * G, Remembered::default());
        let without = budget(fixed.clone(), 4 * G, Remembered::default());
        assert_eq!(
            (with.limit, with.spare),
            (without.limit, without.spare),
            "{ceiling}"
        );
    }
}

#[test]
fn a_ceiling_does_not_change_when_the_host_stalls() {
    // The idle VM stalls 6% of the last 10 s: what it could grow by does not count, so the shares stop starts as on
    // a fixed host, with or without the ceiling.
    for ceiling in [None, Some(24 * G)] {
        let memory = stalled(idle_vm(ceiling), 600);
        assert!(stalls(&memory, false), "stalls with ceiling {ceiling:?}");
        let host = budget(memory, 0, Remembered::default());
        assert!(host.stalled, "stalled with ceiling {ceiling:?}");
        assert_eq!(host.spare, 0, "nothing to spare while it stalls");
    }
    // Once stopped, starts resume only under 1%, ceiling or not.
    let lingering = stalled(idle_vm(Some(24 * G)), 200);
    assert!(!stalls(&lingering, false), "2% does not stop starts");
    assert!(stalls(&lingering, true), "2% keeps them stopped");
}

#[test]
fn with_a_ceiling_a_stall_counts_below_twice_the_margin_of_mem_total() {
    // 5G available on an 8G VM that could grow to 96G: twice the 6G margin of 96G would count the stall, but the
    // stall rule reads the host as it is, whose margin is 2G, so with 5G available the stall is the runners' own.
    let memory = stalled(
        HostMemory {
            ceiling: Some(96 * G),
            ..host(8 * G, 5 * G)
        },
        600,
    );
    assert!(!stalls(&memory, false), "5G is past twice the 2G margin");
}

#[test]
fn a_running_runner_counts_against_the_grown_limit() {
    // The VM grew to 12.8G for one 8G runner, which holds 7G of it; 4G is available and it can grow 11.2G more.
    let memory = HostMemory {
        slice: 7 * G,
        ceiling: Some(24 * G),
        ..host(12 * G + 800 * M, 4 * G)
    };
    let host = budget(memory, 8 * G, Remembered::default());
    assert_eq!(host.limit, 7 * G + 4 * G + (11 * G + 224 * M) - 2 * G);
    assert_eq!(host.spare, host.limit - 8 * G);
    assert!(host.spare < 16 * G, "a second 8G runner does not fit");
}
