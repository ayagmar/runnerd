//! The host's bound on a host whose memory is fixed, and on one that can grow to `[host] memory_ceiling`
//! (ADR 0084): what the slice may hold, the margin it keeps and the stall rule, which a ceiling leaves alone.

use super::{Remembered, budget, first_runner, left, limit, margin, pooled, short, stalls};
use crate::api::{HostMemory, Pressure, Swap};

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
        swap: None,
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

/// A count of `bytes` that takes no credit off.
fn plain(bytes: u64) -> pooled::Count {
    pooled::Count {
        plain: bytes,
        ..pooled::Count::default()
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
        swap: None,
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
            swap: None,
            ..fixed.clone()
        };
        assert_eq!(memory.growable(), 0, "{ceiling}");
        for short in [false, true] {
            assert_eq!(limit(&memory, short), limit(&fixed, short), "{ceiling}");
        }
        let with = budget(memory, plain(4 * G), Remembered::default());
        let without = budget(fixed.clone(), plain(4 * G), Remembered::default());
        assert_eq!(
            (with.limit, with.spare),
            (without.limit, without.spare),
            "{ceiling}"
        );
    }
}

#[test]
fn swap_changes_nothing_admission_counts() {
    // Catches swap room leaking into the host bound (ADR 0101): a host with a free swap device must leave the slice
    // what the same host without one does, stalled or not, short or not.
    for (available, avg10, short) in [(31 * G, 0, false), (31 * G, 0, true), (3 * G, 600, false)] {
        let bare = stalled(host(64 * G, available), avg10);
        let with_swap = HostMemory {
            swap: Some(Swap {
                total: 16 * G,
                free: 16 * G,
            }),
            ..bare.clone()
        };
        let remembered = Remembered::testing(short, false);
        let (without, with) = (
            budget(bare.clone(), plain(8 * G), remembered),
            budget(with_swap.clone(), plain(8 * G), remembered),
        );
        assert_eq!(
            (without.limit, without.spare, without.stalled),
            (with.limit, with.spare, with.stalled),
            "{available} available, {avg10} stalled, short {short}"
        );
        assert_eq!(first_runner(&bare, 0), first_runner(&with_swap, 0));
    }
}

#[test]
fn a_ceiling_does_not_change_when_the_host_stalls() {
    // The idle VM stalls 6% of the last 10 s: what it could grow by does not count, so the shares stop starts as on
    // a fixed host, with or without the ceiling.
    for ceiling in [None, Some(24 * G)] {
        let memory = stalled(idle_vm(ceiling), 600);
        assert!(stalls(&memory, false), "stalls with ceiling {ceiling:?}");
        let host = budget(memory, plain(0), Remembered::default());
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
            swap: None,
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
        swap: None,
        ..host(12 * G + 800 * M, 4 * G)
    };
    let host = budget(memory, plain(8 * G), Remembered::default());
    assert_eq!(host.limit, 7 * G + 4 * G + (11 * G + 224 * M) - 2 * G);
    assert_eq!(host.spare, host.limit - 8 * G);
    assert!(host.spare < 16 * G, "a second 8G runner does not fit");
}

#[test]
fn a_deficit_never_reads_as_a_fit_and_no_waiter_lets_the_margin_go() {
    let memory = host(64 * G, 10 * G);
    // 6G by the plain margin, less the 7G that runs: 1G past it.
    assert!(short(&memory, 7 * G, false, [G]), "a deficit fits nothing");
    assert!(
        short(&memory, 7 * G, false, [0]),
        "not even a job that needs none"
    );
    assert!(
        !short(&memory, 7 * G, false, []),
        "no job waits for the host"
    );
    assert!(
        !short(&memory, 5 * G, false, [3 * G, G]),
        "one of the two fits"
    );
    assert!(short(&memory, 5 * G, true, [G]), "a stall keeps it");
    assert!(short(&memory, 5 * G, true, []), "with no waiter too");
}

mod generated {
    use super::*;

    const CASES: u64 = 4000;

    /// `SplitMix64`: the same seed gives the same sequence on every machine.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A steady reading of a 64G host, with no pressure figures, a stall or a ceiling now and then.
    fn reading(rng: &mut Rng) -> HostMemory {
        let memory = HostMemory {
            slice: rng.below(20) * G,
            ..host(64 * G, rng.below(16 * 4) * G / 4)
        };
        match rng.below(5) {
            0 => HostMemory {
                full: None,
                ..memory
            },
            1 => stalled(memory, 600),
            2 => HostMemory {
                ceiling: Some(96 * G),
                ..memory
            },
            _ => memory,
        }
    }

    /// One plan by what the last remembered: starts the jobs of `queue` that fit, in turn, and remembers the rest.
    /// Returns how many started.
    fn plan(
        memory: &HostMemory,
        remembered: &mut Remembered,
        reserved: &mut u64,
        queue: &mut Vec<u64>,
    ) -> usize {
        let stalled = stalls(memory, remembered.was_stalled());
        let limit = limit(memory, remembered.short() || stalled);
        let queued = queue.len();
        queue.retain(|needs| {
            let fits = left(limit, *reserved, stalled) >= i64::try_from(*needs).unwrap();
            if fits {
                *reserved += needs;
            }
            !fits
        });
        *remembered = Remembered::testing(
            short(memory, *reserved, stalled, queue.iter().copied()),
            stalled,
        );
        queued - queue.len()
    }

    #[test]
    fn on_a_steady_reading_the_band_follows_the_waiters_and_starts_never_alternate_with_holds() {
        for seed in 0..CASES {
            let mut rng = Rng(seed);
            let memory = reading(&mut rng);
            let mut reserved = rng.below(30) * G;
            let mut queue: Vec<u64> = (0..rng.below(9))
                .map(|_| 256 * (1 + rng.below(32)) * M)
                .collect();
            let mut remembered = Remembered::testing(rng.below(2) == 0, false);
            let mut resumed = 0;
            let mut held = false;
            for _ in 0..12 {
                let fitting = !stalls(&memory, remembered.was_stalled())
                    && !remembered.short()
                    && queue.iter().any(|needs| {
                        left(limit(&memory, false), reserved, false)
                            >= i64::try_from(*needs).unwrap()
                    });
                let started = plan(&memory, &mut remembered, &mut reserved, &mut queue);
                assert!(
                    !fitting || started > 0,
                    "seed {seed}: the plan after a waiter fitted the plain margin has it"
                );
                let room = left(limit(&memory, false), reserved, false);
                let fits = queue
                    .iter()
                    .any(|needs| room >= i64::try_from(*needs).unwrap());
                if remembered.was_stalled() {
                    assert!(
                        remembered.short(),
                        "seed {seed}: a stall keeps the larger margin"
                    );
                } else {
                    assert_eq!(
                        remembered.short(),
                        !queue.is_empty() && !fits,
                        "seed {seed}: the larger margin stands while no waiter fits the plain one"
                    );
                }
                resumed += usize::from(held && started > 0);
                held = started == 0;
            }
            assert!(
                resumed <= 1,
                "seed {seed}: starts and holds alternate {resumed} times"
            );
        }
    }
}
