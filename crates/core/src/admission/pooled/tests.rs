//! The pooled count: what it credits, what it never credits, and the bounds it keeps over generated snapshots.

use std::time::Duration;

use super::*;
use crate::admission::host::{self, Remembered};
use crate::api::{HostMemory, JobPeak, Pressure};

const G: u64 = 1 << 30;

const PREDICTED: Basis = Basis::Job {
    runs: 10,
    peak: JobPeak::P95,
};

const SETTLED: Duration = Duration::from_mins(5);

fn key(repository: &str, name: &str) -> Key {
    Key {
        repository: repository.to_owned(),
        workflow: Some("ci".to_owned()),
        name: name.to_owned(),
    }
}

/// A runner of `key` past its first minute, with a prediction to go by.
fn settled(key: &Key, takes: u64, used: u64) -> Runner<'_> {
    Runner {
        key: Some(key),
        takes,
        used: Some(used),
        ran: Some(SETTLED),
        basis: Some(PREDICTED),
    }
}

/// A slice and a host with room for all a credit may be.
fn roomy(runners: &[Runner<'_>]) -> Count {
    count(&Snapshot {
        runners,
        committed: 0,
        slice: Some(Slice {
            memory: 0,
            memory_max: 100 * G,
        }),
        host: Some(Host {
            available: 100 * G,
            keeps: 0,
            stalled: false,
        }),
        most: MOST_CREDIT,
        host_reserve: true,
    })
}

#[test]
fn headroom_of_two_jobs_adds_by_root_sum_square() {
    let (a, b) = (key("o/r", "a"), key("o/r", "b"));
    // 3G and 4G above use come to 5G, not 7G: 2G off.
    let count = roomy(&[settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G)]);
    assert_eq!(
        (count.plain, count.credit, count.runners),
        (10 * G, 2 * G, 2)
    );
    assert_eq!(count.pooled(), 8 * G);
}

#[test]
fn copies_of_one_job_pool_nothing_among_themselves() {
    let job = key("o/r", "backend:it");
    // Across profiles too: a job's key does not say which pool ran it.
    let count = roomy(&[
        settled(&job, 8 * G, G),
        settled(&job, 5 * G, G),
        settled(&job, 2 * G, G),
    ]);
    assert_eq!(count.credit, 0, "copies grow together");
    assert_eq!(count.pooled(), 15 * G);
}

#[test]
fn a_job_of_another_repository_is_another_job() {
    let (a, b) = (key("o/r", "build"), key("o/s", "build"));
    let count = roomy(&[settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G)]);
    assert_eq!(count.credit, 2 * G);
}

#[test]
fn a_runner_under_a_minute_counts_in_full() {
    let (a, b, c) = (key("o/r", "a"), key("o/r", "b"), key("o/r", "c"));
    let just_short = Duration::from_secs(59);
    let young = Runner {
        ran: Some(just_short),
        ..settled(&c, 8 * G, G)
    };
    let without = roomy(&[settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G)]);
    let with = roomy(&[settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G), young]);
    assert_eq!(with.credit, without.credit, "its 7G above use is not room");
    assert_eq!(with.pooled(), without.pooled() + 8 * G);
    let of_age = Runner {
        ran: Some(POOL_AFTER),
        ..young
    };
    let aged = roomy(&[settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G), of_age]);
    assert!(
        aged.credit > with.credit,
        "at a minute it counts on its room"
    );
}

#[test]
fn only_a_prediction_with_a_job_and_a_fresh_sample_is_credited() {
    let (a, b, c) = (key("o/r", "a"), key("o/r", "b"), key("o/r", "c"));
    let lone = roomy(&[settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G)]);
    let third = settled(&c, 8 * G, G);
    let ineligible = [
        Runner { key: None, ..third },
        Runner {
            used: None,
            ..third
        },
        Runner { ran: None, ..third },
        Runner {
            basis: None,
            ..third
        },
        Runner {
            basis: Some(Basis::Cap),
            ..third
        },
        Runner {
            basis: Some(Basis::Unpredictable { until: 9 }),
            ..third
        },
        Runner {
            basis: Some(Basis::Overran { until: 9 }),
            ..third
        },
    ];
    for runner in ineligible {
        let with = roomy(&[settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G), runner]);
        assert_eq!(with.credit, lone.credit, "{runner:?} counts in full");
    }
}

/// `ayagmar/fretboard` and `ayagmar/trellis` at 02:40 UTC on 2026-11-06, from the status of the moment and the last
/// sample before it: what the host study worked out by hand.
#[test]
fn the_status_of_the_night_of_the_study_reproduces_its_figure() {
    const MIRROR: u64 = 536_870_912;
    let it = key("ayagmar/fretboard", "backend / check:backend:it");
    let android = key("ayagmar/trellis", "Android quality gate");
    let infra = key("ayagmar/glowly", "infra-validate");
    let contracts = key("ayagmar/glowly", "contracts-verify");
    let route = key("ayagmar/fretboard", "design-pages (route)");
    let ledger = key("ayagmar/fretboard", "design-pages (ledger)");
    let signal = key("ayagmar/fretboard", "design-pages (signal)");
    let node: u64 = 1_879_048_192;
    let starting = |key, takes, used, ran| Runner {
        ran: Some(Duration::from_secs(ran)),
        ..settled(key, takes, used)
    };
    let runners = [
        settled(&it, 5_368_709_120, 5_205_680_128),
        starting(&it, 8_321_499_136, 510_816_256, 8),
        settled(&android, 11_811_160_064, 8_324_890_624),
        starting(&infra, 536_870_912, 91_303_936, 0),
        starting(&contracts, 536_870_912, 92_073_984, 0),
        settled(&route, node, 1_246_507_008),
        settled(&ledger, node, 946_700_288),
        settled(&signal, node, 1_241_194_496),
        settled(&ledger, node, 999_522_304),
        settled(&signal, node, 1_340_145_664),
        settled(&signal, node, 1_038_569_472),
        settled(&signal, node, 1_009_111_040),
        settled(&ledger, node, 1_061_969_920),
        settled(&route, node, 1_379_262_464),
        settled(&ledger, node, 1_181_487_104),
    ];
    let memory = HostMemory {
        at: 1_793_932_817,
        total: 66_748_674_048,
        available: 26_178_785_280,
        slice: 25_790_750_616,
        some: Some(Pressure {
            avg10: 4,
            avg60: 46,
        }),
        full: Some(Pressure {
            avg10: 3,
            avg60: 39,
        }),
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    };
    let count = count(&Snapshot {
        runners: &runners,
        committed: MIRROR,
        slice: Some(Slice {
            memory: memory.slice,
            memory_max: 49_392_123_904,
        }),
        host: Some(host::pooling(&memory, Remembered::default())),
        most: MOST_CREDIT,
        host_reserve: true,
    });
    let thousandths = |bytes: u64| (bytes * 1000 + G / 2) / G;
    assert_eq!(count.plain, 45_902_462_976, "the status's memory_in_use");
    assert_eq!(thousandths(count.plain), 42_750);
    assert_eq!(thousandths(count.pooled()), 37_849);
    assert_eq!(count.runners, 12, "the three still starting count in full");
}

#[test]
fn every_guard_can_take_the_credit_back() {
    let (a, b) = (key("o/r", "a"), key("o/r", "b"));
    let runners = [settled(&a, 5 * G, 2 * G), settled(&b, 5 * G, G)];
    let snapshot = Snapshot {
        runners: &runners,
        committed: 0,
        slice: Some(Slice {
            memory: 20 * G,
            memory_max: 100 * G,
        }),
        host: Some(Host {
            available: 40 * G,
            keeps: 20 * G,
            stalled: false,
        }),
        most: MOST_CREDIT,
        host_reserve: true,
    };
    assert_eq!(count(&snapshot).credit, 2 * G);
    let credit = |snapshot: Snapshot<'_>| count(&snapshot).credit;
    let slice = |memory| {
        Some(Slice {
            memory,
            memory_max: 100 * G,
        })
    };
    let host = |available, stalled| {
        Some(Host {
            available,
            keeps: 20 * G,
            stalled,
        })
    };
    let no_slice = Snapshot {
        slice: None,
        ..snapshot
    };
    let no_host = Snapshot {
        host: None,
        ..snapshot
    };
    assert_eq!(credit(no_slice), 0, "without a recent slice sample");
    assert_eq!(credit(no_host), 0, "without a recent reading of the host");
    let off = Snapshot {
        most: 0,
        ..snapshot
    };
    let off = count(&off);
    assert_eq!(
        (off.plain, off.credit, off.runners),
        (10 * G, 0, 0),
        "with pooling off"
    );
    // 90G less 87G less the 2G left under the mark.
    let near_the_mark = Snapshot {
        slice: slice(87 * G + G / 2),
        ..snapshot
    };
    assert_eq!(credit(near_the_mark), G / 2);
    let past_the_mark = Snapshot {
        slice: slice(89 * G),
        ..snapshot
    };
    assert_eq!(credit(past_the_mark), 0);
    let short = Snapshot {
        host: host(21 * G, false),
        ..snapshot
    };
    assert_eq!(
        credit(short),
        G,
        "only what the host has past what it keeps"
    );
    let stalled = Snapshot {
        host: host(40 * G, true),
        ..snapshot
    };
    assert_eq!(credit(stalled), 0);
}

#[test]
fn the_host_keeps_room_for_the_two_largest_growths_and_counts_the_rest_at_use() {
    // Catches the host's credit going by another rule than ADR 0108's: a sum, a fixed reserve, the largest job alone,
    // or headroom taken per runner rather than per job.
    let (a, b, c) = (key("o/r", "a"), key("o/r", "b"), key("o/r", "c"));
    // 5G, 4G and 3G of headroom: 12G, of which the two largest keep 9G.
    let three = roomy(&[
        settled(&a, 6 * G, G),
        settled(&b, 6 * G, 2 * G),
        settled(&c, 6 * G, 3 * G),
    ]);
    assert_eq!(three.host_credit, 3 * G);
    assert_eq!(three.hosted(), 18 * G - 3 * G);
    assert_eq!(three.used, 6 * G);
    // Two copies of `c` grow together: one job with 6G of headroom, the largest.
    let copies = roomy(&[
        settled(&a, 6 * G, G),
        settled(&b, 6 * G, 2 * G),
        settled(&c, 6 * G, 3 * G),
        settled(&c, 6 * G, 3 * G),
    ]);
    assert_eq!(copies.host_credit, 15 * G - (6 * G + 5 * G));
    // Under the floor nothing is taken off: 2G, 2G and 1G keep 6G.
    let small = roomy(&[
        settled(&a, 3 * G, G),
        settled(&b, 3 * G, G),
        settled(&c, 2 * G, G),
    ]);
    assert_eq!(small.host_credit, 0, "5G of headroom is under the 6G floor");
    assert_eq!(small.hosted(), small.plain);
    // One job, or two, always keep all of theirs.
    for runners in [
        &[settled(&a, 12 * G, G)][..],
        &[settled(&a, 12 * G, G), settled(&b, 12 * G, G)],
    ] {
        assert_eq!(roomy(runners).host_credit, 0, "{runners:?}");
    }
}

#[test]
fn the_host_credit_needs_its_own_switch_fresh_readings_and_no_stall() {
    // Catches a host credit taken from a stale or missing reading, during a stall, with `pooled_host` off, or turned
    // off with `pooled_count`, which is `MemoryMax`'s decision alone.
    let (a, b, c) = (key("o/r", "a"), key("o/r", "b"), key("o/r", "c"));
    let runners = [
        settled(&a, 6 * G, G),
        settled(&b, 6 * G, 2 * G),
        settled(&c, 6 * G, 3 * G),
    ];
    let snapshot = Snapshot {
        runners: &runners,
        committed: 0,
        slice: Some(Slice {
            memory: 6 * G,
            memory_max: 100 * G,
        }),
        host: Some(Host {
            available: 40 * G,
            keeps: 20 * G,
            stalled: false,
        }),
        most: MOST_CREDIT,
        host_reserve: true,
    };
    let host_credit = |snapshot: Snapshot<'_>| count(&snapshot).host_credit;
    assert_eq!(host_credit(snapshot), 3 * G);
    let pooled_count_off = Snapshot {
        most: 0,
        ..snapshot
    };
    assert_eq!(
        host_credit(pooled_count_off),
        3 * G,
        "apart from MemoryMax's"
    );
    let off = Snapshot {
        host_reserve: false,
        ..snapshot
    };
    let no_slice = Snapshot {
        slice: None,
        ..snapshot
    };
    let no_host = Snapshot {
        host: None,
        ..snapshot
    };
    let stalled = Snapshot {
        host: Some(Host {
            stalled: true,
            ..snapshot.host.unwrap()
        }),
        ..snapshot
    };
    for (gate, snapshot) in [
        ("pooled_host off", off),
        ("no recent slice sample", no_slice),
        ("no recent host reading", no_host),
        ("a stall", stalled),
    ] {
        assert_eq!(host_credit(snapshot), 0, "{gate}");
    }
    // Unlike MemoryMax's credit, the host's is not held to what the host has past a quarter of it: the owner chose
    // to spend that buffer (ADR 0108).
    let short = Snapshot {
        host: Some(Host {
            available: 21 * G,
            ..snapshot.host.unwrap()
        }),
        ..snapshot
    };
    assert_eq!(host_credit(short), 3 * G);
}

#[test]
fn a_host_short_of_memory_keeps_more_of_it() {
    let memory = HostMemory {
        at: 0,
        total: 64 * G,
        available: 30 * G,
        slice: 0,
        some: None,
        full: None,
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    };
    let plain = host::pooling(&memory, Remembered::default());
    assert_eq!(plain.keeps, 4 * G + 16 * G, "the margin and a quarter");
    let short = host::pooling(
        &memory,
        Remembered {
            short: true,
            ..Remembered::default()
        },
    );
    assert_eq!(short.keeps, 6 * G + 16 * G);
    assert_eq!(short.available, 30 * G);
    let growing = host::pooling(
        &HostMemory {
            ceiling: Some(128 * G),
            ..memory
        },
        Remembered::default(),
    );
    assert_eq!(
        growing.available,
        30 * G + 64 * G,
        "what it can still grow by"
    );
    assert_eq!(growing.keeps, 8 * G + 32 * G);
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

    /// Up to twelve runners over five jobs, each with what a real one may lack.
    fn runners<'run>(rng: &mut Rng, keys: &'run [Key]) -> Vec<Runner<'run>> {
        (0..rng.below(13))
            .map(|_| {
                let takes = rng.below(12 * G);
                let used = rng.below(takes + 1);
                let index = usize::try_from(rng.below(keys.len() as u64)).unwrap();
                Runner {
                    key: (rng.below(6) != 0).then(|| &keys[index]),
                    takes,
                    used: (rng.below(6) != 0).then_some(used),
                    ran: match rng.below(6) {
                        0 => None,
                        1 => Some(Duration::from_secs(rng.below(60))),
                        _ => Some(POOL_AFTER + Duration::from_secs(rng.below(3600))),
                    },
                    basis: match rng.below(8) {
                        0 => None,
                        1 => Some(Basis::Cap),
                        2 => Some(Basis::Overran { until: 1 }),
                        3 => Some(Basis::Unpredictable { until: 1 }),
                        4 => Some(Basis::Profile { runs: 9 }),
                        5 => Some(Basis::ProfileMax { runs: 9 }),
                        _ => Some(PREDICTED),
                    },
                }
            })
            .collect()
    }

    fn snapshot<'run>(rng: &mut Rng, runners: &'run [Runner<'run>]) -> Snapshot<'run> {
        Snapshot {
            runners,
            committed: rng.below(2 * G),
            slice: (rng.below(8) != 0).then(|| Slice {
                memory: rng.below(46 * G),
                memory_max: 46 * G,
            }),
            host: (rng.below(8) != 0).then(|| Host {
                available: rng.below(62 * G),
                keeps: 20 * G,
                stalled: rng.below(8) == 0,
            }),
            most: if rng.below(8) == 0 { 0 } else { MOST_CREDIT },
            host_reserve: rng.below(8) != 0,
        }
    }

    /// What the count may never read under: what eligible runners use, with all the rest in full.
    fn floor(snapshot: &Snapshot<'_>) -> u64 {
        let held: u64 = snapshot
            .runners
            .iter()
            .map(|runner| runner.pooled().unwrap_or(runner.takes))
            .sum();
        held + snapshot.committed
    }

    #[test]
    fn the_credit_keeps_its_bounds_on_every_snapshot() {
        let keys: Vec<Key> = (0..5)
            .map(|job| key("o/r", &format!("job-{job}")))
            .collect();
        let mut credited = 0;
        let mut host_credited = 0;
        for seed in 0..CASES {
            let mut rng = Rng(seed);
            let runners = runners(&mut rng, &keys);
            let snapshot = snapshot(&mut rng, &runners);
            let counted = count(&snapshot);
            credited += u64::from(counted.credit > 0);
            assert!(
                counted.credit <= snapshot.most,
                "seed {seed}: {counted:?} past what it may be"
            );
            assert!(
                counted.pooled() >= floor(&snapshot),
                "seed {seed}: {counted:?} under use and what counts in full"
            );
            let slice = snapshot.slice.map_or(0, slice_room);
            let host = snapshot.host.map_or(0, host_room);
            assert!(
                counted.credit <= slice.min(host),
                "seed {seed}: {counted:?} past the guards' {slice} and {host}"
            );
            assert_eq!(
                counted.plain,
                runners.iter().map(|runner| runner.takes).sum::<u64>() + snapshot.committed,
                "seed {seed}: the plain count adds every take"
            );
            host_credited += u64::from(counted.host_credit > 0);
            let headroom: u64 = runners
                .iter()
                .filter_map(|runner| Some(runner.takes - runner.pooled()?))
                .sum();
            let read = snapshot.slice.is_some() && snapshot.host.is_some_and(|host| !host.stalled);
            assert!(
                counted.hosted() <= counted.plain,
                "seed {seed}: {counted:?} past the plain count"
            );
            assert!(
                counted.hosted() >= floor(&snapshot) + headroom.min(HOST_RESERVE_FLOOR),
                "seed {seed}: {counted:?} under use, what counts in full and the floor of {headroom}"
            );
            assert!(
                counted.hosted() >= counted.used,
                "seed {seed}: {counted:?} keeps less than nothing for growth"
            );
            assert!(
                counted.host_credit == 0 || (snapshot.host_reserve && read),
                "seed {seed}: {counted:?} without its switch or a reading"
            );
        }
        assert!(
            credited > CASES / 10,
            "only {credited} snapshots had a credit"
        );
        assert!(
            host_credited > CASES / 20,
            "only {host_credited} snapshots had a host credit"
        );
    }

    /// `Backfill` adds `host_credit_room` to what the host's limit leaves, so an owner's room is never found later than
    /// it can come. With no runner starting or ending, the runners may use anything up to what they take and all grow
    /// eligible; the host's credit then never passes what it was and its room. A room from what they use now, as the
    /// host study first had it, fails here: a runner that shrinks raises the credit past it.
    #[test]
    fn the_host_credit_never_grows_past_its_room_while_the_same_runners_run() {
        let keys: Vec<Key> = (0..5)
            .map(|job| key("o/r", &format!("job-{job}")))
            .collect();
        for seed in 0..CASES {
            let mut rng = Rng(seed);
            let runners = runners(&mut rng, &keys);
            let view = snapshot(&mut rng, &runners);
            let now = count(&view);
            for _ in 0..8 {
                let later: Vec<Runner<'_>> = runners
                    .iter()
                    .map(|runner| Runner {
                        key: runner.key.or(Some(&keys[0])),
                        used: Some(rng.below(runner.takes + 1)),
                        ran: Some(POOL_AFTER),
                        basis: Some(PREDICTED),
                        ..*runner
                    })
                    .collect();
                let after = count(&Snapshot {
                    runners: &later,
                    slice: view.slice.or(Some(Slice {
                        memory: 0,
                        memory_max: 46 * G,
                    })),
                    host: Some(Host {
                        available: 62 * G,
                        keeps: 20 * G,
                        stalled: false,
                    }),
                    ..view
                });
                assert!(
                    after.host_credit <= now.host_credit + now.host_credit_room,
                    "seed {seed}: {now:?} grew to {after:?}"
                );
            }
        }
    }

    /// `Left::take` and `Left::give` work out a plan's starts and a warm runner's stop from the count at the plan.
    #[test]
    fn a_start_or_a_stop_of_what_counts_in_full_moves_the_count_byte_for_byte() {
        let keys: Vec<Key> = (0..5)
            .map(|job| key("o/r", &format!("job-{job}")))
            .collect();
        for seed in 0..CASES {
            let mut rng = Rng(seed);
            let before = runners(&mut rng, &keys);
            let view = snapshot(&mut rng, &before);
            let takes = rng.below(8 * G);
            let starting = Runner {
                key: None,
                takes,
                used: None,
                ran: None,
                basis: None,
            };
            let mut after = before.clone();
            after.push(starting);
            let with = count(&Snapshot {
                runners: &after,
                ..view
            });
            assert_eq!(with.pooled(), count(&view).pooled() + takes, "seed {seed}");
            assert_eq!(with.credit, count(&view).credit, "seed {seed}");
            assert_eq!(with.hosted(), count(&view).hosted() + takes, "seed {seed}");
        }
    }

    /// With the guards as they are: live, a runner's end loosens them and the credit can rise, up to `MOST_CREDIT`, which
    /// is what `Backfill` allows for. This holds that the raw credit does not rise by itself, and that a runner frees at
    /// most what it takes.
    #[test]
    fn a_runner_leaving_frees_no_more_than_it_took_and_never_raises_the_count() {
        let keys: Vec<Key> = (0..5)
            .map(|job| key("o/r", &format!("job-{job}")))
            .collect();
        for seed in 0..CASES {
            let mut rng = Rng(seed);
            let runners = runners(&mut rng, &keys);
            let view = snapshot(&mut rng, &runners);
            let before = count(&view);
            for leaving in 0..runners.len() {
                let mut rest = runners.clone();
                let gone = rest.remove(leaving);
                let after = count(&Snapshot {
                    runners: &rest,
                    ..view
                });
                assert!(
                    after.credit <= before.credit,
                    "seed {seed}: {gone:?} leaving raised the credit, {before:?} to {after:?}"
                );
                assert!(
                    after.pooled() <= before.pooled(),
                    "seed {seed}: {gone:?} leaving raised the count, {before:?} to {after:?}"
                );
                assert!(
                    before.pooled() - after.pooled() <= gone.takes,
                    "seed {seed}: {gone:?} left, {before:?} to {after:?}"
                );
                assert!(
                    after.hosted() <= before.hosted()
                        && before.hosted() - after.hosted() <= gone.takes,
                    "seed {seed}: {gone:?} left, the host's count {before:?} to {after:?}"
                );
            }
        }
    }
}
