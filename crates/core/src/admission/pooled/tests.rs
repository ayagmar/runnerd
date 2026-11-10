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
        used: Reading::Used(used),
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
            memory_max: Some(100 * G),
        }),
        host: Some(Host {
            available: 100 * G,
            keeps: 0,
            stalled: false,
        }),
        most: MOST_CREDIT,
        host_reserve: true,
        growth: None,
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
            used: Reading::Stale,
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
/// The jobs of the night of the study, 2026-11-04, by the name `Night::runners` gives each.
struct Night {
    keys: [Key; 7],
}

const MIRROR: u64 = 536_870_912;

impl Night {
    fn new() -> Night {
        Night {
            keys: [
                key("ayagmar/fretboard", "backend / check:backend:it"),
                key("ayagmar/trellis", "Android quality gate"),
                key("ayagmar/glowly", "infra-validate"),
                key("ayagmar/glowly", "contracts-verify"),
                key("ayagmar/fretboard", "design-pages (route)"),
                key("ayagmar/fretboard", "design-pages (ledger)"),
                key("ayagmar/fretboard", "design-pages (signal)"),
            ],
        }
    }

    /// The fifteen runners of its status, the runner set every count is read on here: three young, the rest
    /// settled.
    fn runners(&self) -> Vec<Runner<'_>> {
        let [it, android, infra, contracts, route, ledger, signal] = &self.keys;
        let node: u64 = 1_879_048_192;
        let starting = |key, takes, used, ran| Runner {
            ran: Some(Duration::from_secs(ran)),
            ..settled(key, takes, used)
        };
        vec![
            settled(it, 5_368_709_120, 5_205_680_128),
            starting(it, 8_321_499_136, 510_816_256, 8),
            settled(android, 11_811_160_064, 8_324_890_624),
            starting(infra, 536_870_912, 91_303_936, 0),
            starting(contracts, 536_870_912, 92_073_984, 0),
            settled(route, node, 1_246_507_008),
            settled(ledger, node, 946_700_288),
            settled(signal, node, 1_241_194_496),
            settled(ledger, node, 999_522_304),
            settled(signal, node, 1_340_145_664),
            settled(signal, node, 1_038_569_472),
            settled(signal, node, 1_009_111_040),
            settled(ledger, node, 1_061_969_920),
            settled(route, node, 1_379_262_464),
            settled(ledger, node, 1_181_487_104),
        ]
    }
}

/// The host of that status.
fn night_memory() -> HostMemory {
    HostMemory {
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
    }
}

/// `runners` read as that night's status read them, counting by the growth count of the `largest` growths, as the
/// density study had it (`Growth::top`).
fn night<'run>(runners: &'run [Runner<'run>], largest: Option<usize>) -> Snapshot<'run> {
    let memory = night_memory();
    Snapshot {
        runners,
        committed: MIRROR,
        slice: Some(Slice {
            memory: memory.slice,
            memory_max: Some(49_392_123_904),
        }),
        host: Some(host::pooling(&memory, Remembered::default())),
        most: MOST_CREDIT,
        host_reserve: true,
        growth: largest.map(Growth::top),
    }
}

fn thousandths(bytes: u64) -> u64 {
    (bytes * 1000 + G / 2) / G
}

#[test]
fn the_status_of_the_night_of_the_study_reproduces_its_figure() {
    let night_jobs = Night::new();
    let runners = night_jobs.runners();
    let count = count(&night(&runners, None));
    assert_eq!(count.plain, 45_902_462_976, "the status's memory_in_use");
    assert_eq!(thousandths(count.plain), 42_750);
    assert_eq!(thousandths(count.pooled()), 37_849);
    assert_eq!(count.runners, 12, "the three still starting count in full");
}

#[test]
fn the_growth_count_reads_what_runs_uses_and_room_for_its_largest_growths() {
    let night_jobs = Night::new();
    let runners = night_jobs.runners();
    // 25.67G used and the mirror; the young `it` runner has 7.27G to grow, Android 3.25G, the two next 0.87G and 0.82G.
    for (largest, kept, counted) in [
        (3, 12_229_300_224, 35_796),
        (2, 11_296_952_320, 34_927),
        (1, 7_810_682_880, 31_681),
    ] {
        let count = count(&night(&runners, Some(largest)));
        assert_eq!(count.kept, Some(kept), "top {largest}");
        assert_eq!(thousandths(count.pooled()), counted, "top {largest}");
        assert_eq!(
            count.hosted(),
            count.pooled(),
            "both budgets read one count"
        );
        assert_eq!(
            thousandths(count.plain),
            42_750,
            "the plain count stays as it is"
        );
    }
}

#[test]
fn a_start_costs_only_what_it_adds_to_the_largest_growths() {
    let night_jobs = Night::new();
    let mut runners = night_jobs.runners();
    let before = count(&night(&runners, Some(GROWTH_JOBS))).pooled();
    let starting = |takes| Runner {
        key: None,
        takes,
        used: Reading::NotYet,
        ran: Some(Duration::ZERO),
        basis: None,
    };
    runners.push(starting(G / 2));
    let small = count(&night(&runners, Some(GROWTH_JOBS)));
    assert_eq!(
        small.pooled(),
        before,
        "0.5G is not among the three largest"
    );
    assert_eq!(
        small.plain,
        45_902_462_976 + G / 2,
        "though it counts in the plain count"
    );
    runners.push(starting(4 * G));
    let large = count(&night(&runners, Some(GROWTH_JOBS))).pooled();
    assert_eq!(
        large - before,
        4 * G - 932_347_904,
        "4G takes the third largest's place"
    );
}

#[test]
fn a_runner_whose_samples_say_nothing_counts_its_whole_take() {
    let night_jobs = Night::new();
    let mut runners = night_jobs.runners();
    // The young `it` runner's sample goes stale: its 8.32G counts in full, and the three largest growths left, 3.25G,
    // 0.87G and 0.82G, are under the floor.
    runners[1].used = Reading::Stale;
    let count = count(&night(&runners, Some(GROWTH_JOBS)));
    assert_eq!(count.kept, Some(GROWTH_FLOOR));
    assert_eq!(
        count.pooled(),
        MIRROR + 25_669_234_688 - 510_816_256 + 8_321_499_136 + GROWTH_FLOOR
    );
}

#[test]
fn the_growth_count_keeps_at_least_its_floor_and_at_most_every_growth() {
    let (a, b) = (key("o/r", "a"), key("o/r", "b"));
    let few = [settled(&a, 3 * G, G), settled(&b, 3 * G, G)];
    let count_of = |runners: &[Runner<'_>]| {
        count(&Snapshot {
            growth: Some(Growth::top(GROWTH_JOBS)),
            ..night(runners, None)
        })
    };
    assert_eq!(
        count_of(&few).pooled(),
        count_of(&few).plain,
        "4G of growth, under the floor, is all kept"
    );
    let many: Vec<Runner<'_>> = (0..8).map(|_| settled(&a, 2 * G, G)).collect();
    assert_eq!(
        count_of(&many).kept,
        Some(GROWTH_FLOOR),
        "8G of growth, three of 1G: the floor"
    );
    assert_eq!(count_of(&many).pooled(), MIRROR + 8 * G + GROWTH_FLOOR);
}

#[test]
fn the_daemons_growth_credits_stop_at_its_ceiling() {
    // Twelve copies of a matrix job, each taking 6G and using 1G, beside a roomy slice and host: 72G of takes count
    // 12G used and 15G for the three largest growths, 45G less, past what the replay ever credited.
    let matrix = key("o/r", "matrix");
    let copies: Vec<Runner<'_>> = (0..12).map(|_| settled(&matrix, 6 * G, G)).collect();
    let count_by = |growth| {
        count(&Snapshot {
            runners: &copies,
            committed: 0,
            slice: Some(Slice {
                memory: 0,
                memory_max: Some(100 * G),
            }),
            host: Some(Host {
                available: 100 * G,
                keeps: 0,
                stalled: false,
            }),
            most: MOST_CREDIT,
            host_reserve: true,
            growth: Some(growth),
        })
    };
    let open = count_by(Growth {
        ceiling: None,
        ..GROWTH
    });
    assert_eq!((open.credit, open.host_credit), (45 * G, 45 * G));
    let daemon = count_by(GROWTH);
    assert_eq!(
        (daemon.credit, daemon.host_credit),
        (GROWTH_CEILING, GROWTH_CEILING)
    );
    assert_eq!(daemon.pooled(), 72 * G - GROWTH_CEILING);
}

#[test]
fn below_the_guards_line_the_growth_count_is_plain() {
    let night_jobs = Night::new();
    let runners = night_jobs.runners();
    let snapshot = night(&runners, Some(GROWTH_JOBS));
    let keeps = snapshot.host.unwrap().keeps;
    let at = |available, stalled| Snapshot {
        host: Some(Host {
            available,
            keeps,
            stalled,
        }),
        ..snapshot
    };
    let plain = |snapshot: Snapshot<'_>| {
        let count = count(&snapshot);
        count.kept.is_none() && count.pooled() == count.plain && count.hosted() == count.plain
    };
    assert!(
        !plain(at(keeps, false)),
        "at the line it still counts by growth"
    );
    assert!(plain(at(keeps - 1, false)), "under it, plain");
    assert!(plain(at(keeps + G, true)), "while the host stalls");
    assert!(
        plain(Snapshot {
            host: None,
            ..snapshot
        }),
        "without a reading of the host"
    );
    assert!(
        plain(Snapshot {
            slice: None,
            ..snapshot
        }),
        "without a recent slice sample"
    );
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
            memory_max: Some(100 * G),
        }),
        host: Some(Host {
            available: 40 * G,
            keeps: 20 * G,
            stalled: false,
        }),
        most: MOST_CREDIT,
        host_reserve: true,
        growth: None,
    };
    assert_eq!(count(&snapshot).credit, 2 * G);
    let credit = |snapshot: Snapshot<'_>| count(&snapshot).credit;
    let slice = |memory| {
        Some(Slice {
            memory,
            memory_max: Some(100 * G),
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
fn the_host_credit_needs_its_own_reserve_fresh_readings_and_no_stall() {
    // Catches a host credit taken from a stale or missing reading, during a stall, without the host's reserve, taken
    // off with `MemoryMax`'s credit, which is that budget's alone, or lost on a slice with no `MemoryMax`, where the
    // host binds alone.
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
            memory_max: Some(100 * G),
        }),
        host: Some(Host {
            available: 40 * G,
            keeps: 20 * G,
            stalled: false,
        }),
        most: MOST_CREDIT,
        host_reserve: true,
        growth: None,
    };
    let host_credit = |snapshot: Snapshot<'_>| count(&snapshot).host_credit;
    assert_eq!(host_credit(snapshot), 3 * G);
    let no_slice_credit = Snapshot {
        most: 0,
        ..snapshot
    };
    assert_eq!(
        host_credit(no_slice_credit),
        3 * G,
        "apart from MemoryMax's"
    );
    // A slice with no `MemoryMax` leaves `MemoryMax`'s credit nothing, and the host's credit all it had.
    let unlimited = count(&Snapshot {
        slice: Some(Slice {
            memory: 6 * G,
            memory_max: None,
        }),
        ..snapshot
    });
    assert_eq!((unlimited.credit, unlimited.host_credit), (0, 3 * G));
    let off = Snapshot {
        host_reserve: false,
        growth: None,
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
        ("no host reserve", off),
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
    let short = host::pooling(&memory, Remembered::testing(true, false));
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
                    used: match rng.below(8) {
                        0 => Reading::Stale,
                        1 => Reading::NotYet,
                        _ => Reading::Used(used),
                    },
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
                memory_max: Some(46 * G),
            }),
            host: (rng.below(8) != 0).then(|| Host {
                available: rng.below(62 * G),
                keeps: 20 * G,
                stalled: rng.below(8) == 0,
            }),
            most: if rng.below(8) == 0 { 0 } else { MOST_CREDIT },
            host_reserve: rng.below(8) != 0,
            growth: None,
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
                        used: Reading::Used(rng.below(runner.takes + 1)),
                        ran: Some(POOL_AFTER),
                        basis: Some(PREDICTED),
                        ..*runner
                    })
                    .collect();
                let after = count(&Snapshot {
                    runners: &later,
                    slice: view.slice.or(Some(Slice {
                        memory: 0,
                        memory_max: Some(46 * G),
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
                used: Reading::Stale,
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

    /// `runners` in a generated snapshot counted by a growth count of one to four jobs, each of its rules on or off,
    /// its brake holding it one time in eight: the daemon's among them.
    fn growth_snapshot<'run>(rng: &mut Rng, runners: &'run [Runner<'run>]) -> Snapshot<'run> {
        let growth = Growth {
            jobs: 1 + usize::try_from(rng.below(4)).unwrap(),
            bound: if rng.below(2) == 0 {
                Bound::HighWater
            } else {
                Bound::Open
            },
            young: if rng.below(2) == 0 {
                Young::Covered
            } else {
                Young::Ranked
            },
            below: if rng.below(2) == 0 {
                Below::Credits
            } else {
                Below::Plain
            },
            hold: if rng.below(8) == 0 {
                Hold::Held
            } else {
                Hold::Free
            },
            ceiling: (rng.below(3) == 0).then(|| rng.below(20 * G)),
        };
        Snapshot {
            growth: Some(growth),
            ..snapshot(rng, runners)
        }
    }

    /// The growth count, under the guard's line or over it, by every rule: never past the plain count; over the line
    /// and free, what counts in full with the room it keeps, which is at least the floor and with `Young::Covered`
    /// every young runner's growth, at most all of it, and at least the plain count less any ceiling, both budgets
    /// reading that unbounded and at least that bounded;
    /// otherwise what `Below` says. A runner leaving frees at most its take; a start adds at most its own.
    #[test]
    fn the_growth_count_keeps_its_bounds_on_every_snapshot() {
        let keys: Vec<Key> = (0..5)
            .map(|job| key("o/r", &format!("job-{job}")))
            .collect();
        let mut counted = 0;
        for seed in 0..CASES {
            let mut rng = Rng(seed);
            let runners = runners(&mut rng, &keys);
            let view = growth_snapshot(&mut rng, &runners);
            let growth = view.growth.unwrap();
            let now = count(&view);
            counted += u64::from(now.kept.is_some());
            assert!(
                now.pooled() <= now.plain && now.hosted() <= now.plain,
                "seed {seed}: {now:?}"
            );
            if let Some(kept) = now.kept {
                let headroom: u64 = runners.iter().filter_map(Runner::headroom).sum();
                let young: u64 = runners
                    .iter()
                    .filter(|runner| runner.young())
                    .filter_map(Runner::headroom)
                    .sum();
                let full: u64 = runners
                    .iter()
                    .map(|runner| match runner.used {
                        Reading::Used(_) => runner.sampled().unwrap_or(0),
                        Reading::NotYet => 0,
                        Reading::Stale => runner.takes,
                    })
                    .sum();
                // A ceiling keeps each credit at most it, and so each count at least the plain count less it.
                let growth_count = (view.committed + full + kept).max(
                    growth
                        .ceiling
                        .map_or(0, |ceiling| now.plain.saturating_sub(ceiling)),
                );
                match growth.bound {
                    Bound::Open => assert!(
                        now.pooled() == growth_count && now.hosted() == growth_count,
                        "seed {seed}: {now:?}"
                    ),
                    Bound::HighWater => assert!(
                        now.pooled() >= growth_count
                            && now.hosted() >= growth_count
                            && now.pooled() >= now.hosted(),
                        "seed {seed}: bounded, {now:?}"
                    ),
                }
                assert!(
                    kept >= headroom.min(GROWTH_FLOOR) && kept <= headroom,
                    "seed {seed}: kept {kept} of {headroom}"
                );
                if growth.young == Young::Covered {
                    assert!(kept >= young, "seed {seed}: kept {kept}, young {young}");
                }
            } else {
                let below = match growth.below {
                    Below::Plain => Count {
                        plain: now.plain,
                        ..Count::default()
                    },
                    Below::Credits => count(&Snapshot {
                        growth: None,
                        ..view
                    }),
                };
                assert!(
                    now.pooled() == below.pooled() && now.hosted() == below.hosted(),
                    "seed {seed}: below the line or held, {now:?} against {below:?}"
                );
            }
            one_leaving_or_starting_moves_it_by_its_take(seed, &mut rng, &view, now);
        }
        assert!(
            counted > CASES / 3,
            "only {counted} snapshots were over the guard's line"
        );
    }

    /// Part of `the_growth_count_keeps_its_bounds_on_every_snapshot`: a runner leaving `view` frees at most its take, and
    /// a start adds at most its own, from the count `now`.
    fn one_leaving_or_starting_moves_it_by_its_take(
        seed: u64,
        rng: &mut Rng,
        view: &Snapshot<'_>,
        now: Count,
    ) {
        let runners = view.runners;
        for leaving in 0..runners.len() {
            let mut rest = runners.to_vec();
            let gone = rest.remove(leaving);
            let after = count(&Snapshot {
                runners: &rest,
                ..*view
            });
            for read in [Count::pooled, Count::hosted] {
                assert!(
                    read(after) <= read(now) && read(now) - read(after) <= gone.takes,
                    "seed {seed}: {gone:?} left, {now:?} to {after:?}"
                );
            }
        }
        let takes = rng.below(8 * G);
        let mut started = runners.to_vec();
        started.push(Runner {
            key: None,
            takes,
            used: Reading::NotYet,
            ran: Some(Duration::ZERO),
            basis: None,
        });
        let with = count(&Snapshot {
            runners: &started,
            ..*view
        });
        for read in [Count::pooled, Count::hosted] {
            assert!(
                read(with) >= read(now) && read(with) - read(now) <= takes,
                "seed {seed}: a start of {takes} took {now:?} to {with:?}"
            );
        }
    }

    /// `Backfill` reads `credit_room` and `host_credit_room`: with the same runners, using anything up to what they
    /// take, beside any slice and a host over the line, neither count falls further than its room, held or not.
    #[test]
    fn the_growth_count_never_falls_past_its_room_while_the_same_runners_run() {
        let keys: Vec<Key> = (0..5)
            .map(|job| key("o/r", &format!("job-{job}")))
            .collect();
        for seed in 0..CASES {
            let mut rng = Rng(seed);
            let runners = runners(&mut rng, &keys);
            let view = growth_snapshot(&mut rng, &runners);
            let now = count(&view);
            for _ in 0..4 {
                let later: Vec<Runner<'_>> = runners
                    .iter()
                    .map(|runner| Runner {
                        used: Reading::Used(rng.below(runner.takes + 1)),
                        ..*runner
                    })
                    .collect();
                let after = count(&Snapshot {
                    runners: &later,
                    slice: view.slice.or(Some(Slice {
                        memory: 0,
                        memory_max: Some(46 * G),
                    })),
                    host: Some(Host {
                        available: 62 * G,
                        keeps: 20 * G,
                        stalled: false,
                    }),
                    ..view
                });
                assert!(
                    after.pooled() + now.credit_room >= now.pooled()
                        && after.hosted() + now.host_credit_room >= now.hosted(),
                    "seed {seed}: {now:?} fell to {after:?}"
                );
            }
        }
    }
}
