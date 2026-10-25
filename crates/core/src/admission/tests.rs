//! Predictions and replays over synthetic histories: two profiles of a 33G slice, jobs of fictional repositories.

use std::collections::BTreeMap;

use super::{
    Basis, Copies, HIGH_WATER, JobPeak, Key, Policy, Predictor, Replay, UNPREDICTABLE_FOR, simulate,
};
use crate::config::Config;
use crate::sizing::{Measure, Ran};

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;
const MINUTE: u64 = 60 * 1000;
const NOW: u64 = 1_800_000_000_000;

/// `jvm` 2 × 9G and `node` 2 × 7G, with the 512M mirror 32.5G of a 33G slice.
const CONFIG: &str = r#"
[host]
slice = "runnerd.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = true, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.jvm]
image_stage = "jvm"
cpus = 8
memory = "9G"
slots = 2

[profiles.node]
image_stage = "node"
cpus = 8
memory = "7G"
slots = 2

[[repositories]]
name = "halyard/ledger"
profile = "jvm"
labels = ["jvm"]

[[repositories]]
name = "halyard/storefront"
profile = "node"
labels = ["node"]
"#;

fn config() -> Config {
    Config::parse(CONFIG).unwrap()
}

/// A `name` job of `repository` in `profile`, queued at `queued` (ms), running `minutes`, peaking at `peak`.
fn run(repository: &str, name: &str, profile: &str, queued: u64, minutes: u64, peak: u64) -> Ran {
    Ran {
        repository: repository.to_owned(),
        workflow: Some("CI".to_owned()),
        name: name.to_owned(),
        profile: profile.to_owned(),
        memory_cap: Some(if profile == "jvm" { 9 * G } else { 7 * G }),
        cpu_cap: None,
        memory_peak: peak,
        cpu_peak: 3000,
        cpu_average: Some(1000),
        queued_at: queued,
        started_at: Some(queued),
        runner_started_at: queued,
        finished_at: Some(queued + minutes * MINUTE),
        oom_killed: false,
        reserved: None,
        host_waited: 0,
        hit_cap: Some(false),
        measure: Measure::WithoutPageCache,
        succeeded: true,
    }
}

fn build(peak: u64) -> Ran {
    run("halyard/storefront", "build", "node", NOW, 5, peak)
}

fn predictor() -> Predictor {
    Predictor::new(&config().sizing)
}

fn key(name: &str) -> Key {
    Key {
        repository: "halyard/storefront".to_owned(),
        workflow: Some("CI".to_owned()),
        name: name.to_owned(),
    }
}

#[test]
fn a_job_with_enough_runs_is_predicted_from_its_own_p95() {
    let mut predictor = predictor();
    for i in 0..20 {
        predictor.learn(&build(1500 * M + i * 25 * M), "node", 7 * G, NOW);
    }
    // p95 of 1500M … 1975M is 1950M; + 15% is 2242.5M, rounded up to 2304M.
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Job {
            runs: 20,
            peak: JobPeak::P95
        }
    );
    assert_eq!(prediction.memory, 2304 * M);
}

#[test]
fn a_job_with_one_run_takes_its_profiles_p95_or_its_peak_whichever_is_more() {
    let mut predictor = predictor();
    for _ in 0..60 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    predictor.learn(
        &run("halyard/storefront", "e2e", "node", NOW, 20, 5 * G),
        "node",
        7 * G,
        NOW,
    );
    predictor.learn(
        &run("halyard/storefront", "lint", "node", NOW, 1, 200 * M),
        "node",
        7 * G,
        NOW,
    );
    // The profile's p95 is 2G, but e2e reached 5G: 5.75G.
    let e2e = predictor.predict(&key("e2e"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(e2e.basis, Basis::Profile { runs: 62 });
    assert_eq!(e2e.memory, 5888 * M);
    // One light run may not be the job's heaviest: 2G + 15%, rounded up to 2.5G.
    let lint = predictor.predict(&key("lint"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(lint.basis, Basis::Profile { runs: 62 });
    assert_eq!(lint.memory, 2560 * M);
}

#[test]
fn a_job_with_a_few_runs_reserves_its_own_largest_peak_even_above_its_profiles_p95() {
    let mut predictor = predictor();
    for _ in 0..60 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    for peak in [4 * G, 5 * G, 4 * G] {
        predictor.learn(
            &run("halyard/storefront", "e2e", "node", NOW, 20, peak),
            "node",
            7 * G,
            NOW,
        );
    }
    // A rare heavy job is not predicted at its light neighbours' 2G: 5G + 15% is 5.75G.
    let prediction = predictor.predict(&key("e2e"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Job {
            runs: 3,
            peak: JobPeak::Largest
        }
    );
    assert_eq!(prediction.memory, 5888 * M);
}

#[test]
fn a_job_whose_own_runs_all_peaked_far_below_its_profiles_p95_reserves_near_its_own_peak() {
    // A profile of 200M checks and 4.8G stacks: its p95 is 4.8G.
    let predictor = young_build(&vec![build(1300 * M); 6]);
    // 1300M + 15% is 1495M, rounded up to 1.5G, not the 5.5G of the profile's p95.
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Job {
            runs: 6,
            peak: JobPeak::Largest
        }
    );
    assert_eq!(prediction.memory, 1536 * M);
}

/// A predictor that learned 30 runs of a profile whose p95 is 4.8G, then `runs` of a 1.3G build.
fn young_build(runs: &[Ran]) -> Predictor {
    let mut predictor = predictor();
    for i in 0..30 {
        let (name, peak) = if i % 3 == 0 {
            ("e2e", 4800 * M)
        } else {
            ("changes", 200 * M)
        };
        predictor.learn(
            &run("halyard/storefront", name, "node", NOW, 5, peak),
            "node",
            7 * G,
            NOW,
        );
    }
    for run in runs {
        predictor.learn(run, "node", 7 * G, NOW);
    }
    predictor
}

#[test]
fn failed_runs_do_not_let_a_young_job_go_by_its_own_peaks() {
    // Two runs that failed before their heaviest step, and one that succeeded.
    let failed = Ran {
        succeeded: false,
        ..build(300 * M)
    };
    let predictor = young_build(&[failed.clone(), failed, build(1300 * M)]);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW, Copies::Alone);
    // The profile's 4.8G + 15%, rounded up to 5.5G.
    assert_eq!(prediction.basis, Basis::Profile { runs: 33 });
    assert_eq!(prediction.memory, 5632 * M);
}

#[test]
fn a_copy_of_a_young_job_beside_another_takes_its_profiles_p95() {
    let predictor = young_build(&[build(1300 * M), build(1300 * M)]);
    let alone = predictor.predict(&key("build"), "node", 7 * G, NOW, Copies::Alone);
    let beside = predictor.predict(&key("build"), "node", 7 * G, NOW, Copies::Several);
    assert_eq!((alone.memory, beside.memory), (1536 * M, 5632 * M));
    assert_eq!(beside.basis, Basis::Profile { runs: 32 });
}

#[test]
fn a_job_never_seen_reserves_the_largest_peak_of_its_profile() {
    let mut predictor = predictor();
    for _ in 0..60 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    predictor.learn(
        &run("halyard/storefront", "e2e", "node", NOW, 20, 5 * G),
        "node",
        7 * G,
        NOW,
    );
    // A new workflow may be the profile's heaviest: 5G + 15%, not the p95's 2G.
    let prediction = predictor.predict(&key("lint"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(prediction.basis, Basis::ProfileMax { runs: 61 });
    assert_eq!(prediction.memory, 5888 * M);
}

#[test]
fn a_run_past_what_it_reserved_makes_its_job_reserve_its_largest_peak_for_a_week() {
    let mut predictor = predictor();
    for _ in 0..19 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    // Reserved 2.5G, peaked at 4G: under the cap, so only what it reserved shows it ran over.
    let overran = Ran {
        reserved: Some(2560 * M),
        ..build(4 * G)
    };
    predictor.learn(&overran, "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Overran {
            until: (NOW + UNPREDICTABLE_FOR) / 1000
        }
    );
    // 4G + 15% is 4.6G, rounded up to 4.75G; its p95 alone would say 2.5G.
    assert_eq!(prediction.memory, 4864 * M);
    assert_eq!(
        predictor
            .predict(
                &key("build"),
                "node",
                7 * G,
                NOW + UNPREDICTABLE_FOR,
                Copies::Alone
            )
            .basis,
        Basis::Job {
            runs: 20,
            peak: JobPeak::P95
        }
    );
}

#[test]
fn a_cancelled_run_killed_for_memory_makes_its_job_unpredictable_without_counting_its_peak() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    let killed = Ran {
        oom_killed: true,
        ..build(0)
    };
    predictor.mark(&killed, "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Unpredictable {
            until: (NOW + UNPREDICTABLE_FOR) / 1000
        }
    );
    // Its peak of 0 did not join the job's runs.
    assert_eq!(
        predictor
            .predict(
                &key("build"),
                "node",
                7 * G,
                NOW + UNPREDICTABLE_FOR,
                Copies::Alone
            )
            .basis,
        Basis::Job {
            runs: 20,
            peak: JobPeak::P95
        }
    );
}

#[test]
fn without_enough_runs_of_the_job_or_its_profile_a_job_is_admitted_at_its_cap() {
    let mut predictor = predictor();
    for _ in 0..3 {
        predictor.learn(&build(G), "node", 7 * G, NOW);
    }
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(prediction.basis, Basis::Cap);
    assert_eq!(prediction.memory, 7 * G);
}

#[test]
fn a_run_out_of_memory_at_its_cap_makes_its_job_unpredictable_for_a_week() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    // Reclaim failed at the cap; what it held between samples stayed under 3G.
    let starved = Ran {
        hit_cap: Some(true),
        ..build(3 * G)
    };
    predictor.learn(&starved, "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Unpredictable {
            until: (NOW + UNPREDICTABLE_FOR) / 1000
        }
    );
    assert_eq!(prediction.memory, 7 * G);
    let later = predictor.predict(
        &key("build"),
        "node",
        7 * G,
        NOW + UNPREDICTABLE_FOR,
        Copies::Alone,
    );
    assert_eq!(
        later.basis,
        Basis::Job {
            runs: 21,
            peak: JobPeak::P95
        }
    );
}

#[test]
fn a_job_whose_page_cache_fills_its_cap_is_not_put_back_on_its_cap() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    // One run came close to its cap and the kernel never ran out of memory for it: it reclaimed page cache.
    predictor.learn(&build(6900 * M), "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Job {
            runs: 21,
            peak: JobPeak::P95
        }
    );
    assert!(prediction.memory < 7 * G, "{prediction:?}");
}

#[test]
fn without_its_containers_events_a_run_that_peaked_near_its_cap_makes_its_job_unpredictable() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    let unknown = Ran {
        hit_cap: None,
        ..build(6900 * M)
    };
    predictor.learn(&unknown, "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert!(
        matches!(prediction.basis, Basis::Unpredictable { .. }),
        "{prediction:?}"
    );
}

#[test]
fn a_run_killed_for_memory_makes_its_job_unpredictable_whatever_its_sampled_peak() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    // The kill came between two samples: the peak sampled before it is low.
    let killed = Ran {
        oom_killed: true,
        ..build(2 * G)
    };
    predictor.learn(&killed, "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert!(
        matches!(prediction.basis, Basis::Unpredictable { .. }),
        "{prediction:?}"
    );
    assert_eq!(prediction.memory, 7 * G);
}

#[test]
fn a_prediction_never_exceeds_the_cap() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(6500 * M), "node", 9 * G, NOW);
    }
    assert_eq!(
        predictor
            .predict(&key("build"), "node", 7 * G, NOW, Copies::Alone)
            .memory,
        7 * G
    );
}

/// A day of storefront pushes: a burst of 8 builds every 20 minutes, 2G each for 15 minutes, after a week of
/// history the predictor learned from.
fn bursts() -> Vec<Ran> {
    let mut jobs: Vec<Ran> = (0..20)
        .map(|i| {
            run(
                "halyard/storefront",
                "build",
                "node",
                NOW - 7 * 24 * 60 * MINUTE + i * 30 * MINUTE,
                15,
                2 * G,
            )
        })
        .collect();
    for burst in 0..72 {
        for _ in 0..8 {
            jobs.push(run(
                "halyard/storefront",
                "build",
                "node",
                NOW + burst * 20 * MINUTE,
                15,
                2 * G,
            ));
        }
    }
    jobs
}

#[test]
fn measured_admission_runs_more_bursts_at_once_without_the_peaks_crossing_memory_max() {
    let jobs = bursts();
    let slots = BTreeMap::from([("node".to_owned(), 8)]);
    let caps = simulate(&config(), Some(33 * G), None, &jobs, Policy::Caps, &slots);
    let measured = simulate(
        &config(),
        Some(33 * G),
        None,
        &jobs,
        Policy::Measured,
        &slots,
    );
    // At 7G caps the 32.5G budget holds 4 of a burst; the other 4 wait. Predicted at 2.5G, all 8 run.
    assert_eq!((caps.most_running, measured.most_running), (4, 8));
    assert!(
        measured.wait.p95 < caps.wait.p95,
        "measured waits less: {measured:?} against {caps:?}"
    );
    assert!(measured.worst_overlap <= 33 * G, "{measured:?}");
    assert_eq!(measured.under_predicted, 0);
    assert_eq!((caps.never_started, measured.never_started), (0, 0));
}

#[test]
fn the_high_water_mark_holds_back_what_the_predictions_have_room_for() {
    // Builds that peak at 3G after a history of 1G: the predictions (1.25G) let 30 start, their peaks fill the
    // slice past 90% first.
    let mut jobs: Vec<Ran> = (0..20)
        .map(|i| {
            run(
                "halyard/storefront",
                "build",
                "node",
                NOW - 600 * MINUTE + i * 20 * MINUTE,
                10,
                G,
            )
        })
        .collect();
    jobs.extend((0..30).map(|_| run("halyard/storefront", "build", "node", NOW, 60, 3 * G)));
    let slots = BTreeMap::from([("node".to_owned(), 30)]);
    let measured = simulate(
        &config(),
        Some(33 * G),
        None,
        &jobs,
        Policy::Measured,
        &slots,
    );
    assert!(measured.held_back > 0, "{measured:?}");
    // The budget is 32.5G: high water is 90% of it; one more 3G peak may cross it, never two.
    assert!(
        measured.worst_overlap <= (32 * G + 512 * M) * HIGH_WATER / 100 + 3 * G + 512 * M,
        "{measured:?}"
    );
}

#[test]
fn a_runner_starts_reserving_the_largest_prediction_among_the_waiting_jobs_of_its_repository() {
    let config = config();
    // Ten builds at 1G and ten e2e at 5G, one at a time, long before: 1.25G and 5.75G predicted.
    let mut jobs: Vec<Ran> = (0..20)
        .map(|i| {
            let (name, peak) = if i % 2 == 0 {
                ("build", G)
            } else {
                ("e2e", 5 * G)
            };
            run(
                "halyard/storefront",
                name,
                "node",
                NOW + i * 10 * MINUTE,
                1,
                peak,
            )
        })
        .collect();
    let start = NOW + 300 * MINUTE;
    // An e2e runs for half an hour; a build and another e2e queue a minute in.
    jobs.push(run("halyard/storefront", "e2e", "node", start, 30, 5 * G));
    jobs.push(run(
        "halyard/storefront",
        "build",
        "node",
        start + MINUTE,
        5,
        G,
    ));
    jobs.push(run(
        "halyard/storefront",
        "e2e",
        "node",
        start + MINUTE,
        5,
        5 * G,
    ));
    let slots = BTreeMap::from([("node".to_owned(), 3)]);
    // An 8G budget beside the mirror: the build's runner may get the e2e, so it waits for 5.75G with the first.
    let replay = simulate(
        &config,
        Some(8 * G + 512 * M),
        None,
        &jobs,
        Policy::Measured,
        &slots,
    );
    assert_eq!(replay.never_started, 0);
    // Both waited the 29 minutes; nearest-rank p95 of 23 waits is the 22nd.
    assert_eq!(replay.wait.p95, 29 * 60);
}

#[test]
fn a_cancelled_run_past_what_it_reserved_makes_its_job_reserve_at_least_that_runs_peak() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    // Cancelled after it reached 5G on a 2.5G reservation: its peak is not one of the job's, but it ran over.
    let cancelled = Ran {
        reserved: Some(2560 * M),
        ..build(5 * G)
    };
    predictor.mark(&cancelled, "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert_eq!(
        prediction.basis,
        Basis::Overran {
            until: (NOW + UNPREDICTABLE_FOR) / 1000
        }
    );
    // 5G + 15%, not the 2G its finished runs reached.
    assert_eq!(prediction.memory, 5888 * M);
}

#[test]
fn a_job_seen_only_cancelled_reserves_its_cap() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    predictor.mark(
        &run("halyard/storefront", "e2e", "node", NOW, 3, G),
        "node",
        7 * G,
        NOW,
    );
    let prediction = predictor.predict(&key("e2e"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert_eq!(prediction.basis, Basis::Cap);
    assert_eq!(prediction.memory, 7 * G);
}

/// A profile whose p95 is 4.8G, then two 1.3G builds, long before: a build's own runs say 1.5G, its profile 5.5G.
fn young_history() -> Vec<Ran> {
    let mut jobs: Vec<Ran> = (0..30)
        .map(|i| {
            let (name, peak) = if i % 3 == 0 {
                ("e2e", 4800 * M)
            } else {
                ("changes", 200 * M)
            };
            run(
                "halyard/storefront",
                name,
                "node",
                NOW + i * 10 * MINUTE,
                1,
                peak,
            )
        })
        .collect();
    for i in 0..2 {
        jobs.push(run(
            "halyard/storefront",
            "build",
            "node",
            NOW + (300 + i * 10) * MINUTE,
            1,
            1300 * M,
        ));
    }
    jobs
}

/// `young_history`, then six builds queued at once peaking at `peak`: the first runs 1 minute, the others 30.
fn burst_after_young_history(peak: u64) -> Replay {
    let mut jobs = young_history();
    let burst = NOW + 600 * MINUTE;
    jobs.extend((0..6).map(|i| {
        let minutes = if i == 0 { 1 } else { 30 };
        run("halyard/storefront", "build", "node", burst, minutes, peak)
    }));
    let slots = BTreeMap::from([("node".to_owned(), 6)]);
    simulate(
        &config(),
        Some(20 * G + 512 * M),
        None,
        &jobs,
        Policy::Measured,
        &slots,
    )
}

#[test]
fn a_replayed_burst_of_copies_of_a_young_job_starts_one_by_its_own_runs_and_the_rest_by_its_profile()
 {
    let mut jobs = young_history();
    // Six builds queue at once.
    let burst = NOW + 600 * MINUTE;
    jobs.extend((0..6).map(|_| run("halyard/storefront", "build", "node", burst, 30, 1300 * M)));
    let slots = BTreeMap::from([("node".to_owned(), 6)]);
    let replay = simulate(
        &config(),
        Some(20 * G + 512 * M),
        None,
        &jobs,
        Policy::Measured,
        &slots,
    );
    // The first reserves 1.5G, each other 5.5G: 4 of the 20G budget at once, not the 6 that 1.5G each would fit.
    assert_eq!(replay.most_running, 4);
    assert_eq!(replay.never_started, 0);
}

#[test]
fn once_the_first_copy_ends_the_replay_lets_the_next_running_copy_go_by_its_own_runs() {
    let replay = burst_after_young_history(1300 * M);
    // 1.5G and three 5.5G of the 20G budget run at once. When the first ends a minute in, the next running copy
    // drops to 1.5G, so a fifth starts at 5.5G beside them; the sixth waits for the others' 30 minutes.
    assert_eq!(replay.most_running, 4);
    // Of the 38 jobs replayed, only those two wait.
    assert_eq!((replay.wait.p95, replay.wait.max), (60, 30 * 60));
    assert_eq!(replay.under_predicted, 0);
}

#[test]
fn a_replayed_copy_past_its_own_runs_makes_the_running_copies_reserve_its_peak() {
    let replay = burst_after_young_history(1600 * M);
    // The first copy reserved 1.5G and peaked at 1.6G. The others drop from 5.5G to 1.6G + 15%, rounded up to 2G,
    // for the week after: the two waiting start at 2G beside them.
    assert_eq!(replay.under_predicted, 1);
    assert_eq!(replay.most_running, 5);
    assert_eq!(replay.wait.max, 60);
}

/// A `build` run of halyard/storefront that used `average` millicores over its run, peaking at all 8 CPUs.
fn build_using(average: u64) -> Ran {
    Ran {
        cpu_peak: 8000,
        cpu_average: Some(average),
        ..build(2 * G)
    }
}

#[test]
fn a_job_reserves_the_cpu_it_used_over_its_runs_not_its_peak() {
    let mut predictor = predictor();
    for i in 0..20 {
        predictor.learn(&build_using(1000 + i * 50), "node", 7 * G, NOW);
    }
    // p95 of 1000 … 1950 millicores is 1900; its runs peaked at all 8 CPUs.
    let prediction = predictor.predict_cpu(&key("build"), "node", 8000);
    assert_eq!(
        prediction.basis,
        Basis::Job {
            runs: 20,
            peak: JobPeak::P95
        }
    );
    assert_eq!(prediction.cpu, 1900);
}

#[test]
fn a_job_never_seen_reserves_its_profiles_busiest_average_and_without_history_its_cpus() {
    let mut predictor = predictor();
    assert_eq!(
        predictor.predict_cpu(&key("e2e"), "node", 8000).cpu,
        8000,
        "no runs at all: the profile's cpus"
    );
    for i in 0..10 {
        predictor.learn(&build_using(500 + i * 100), "node", 7 * G, NOW);
    }
    let e2e = predictor.predict_cpu(&key("e2e"), "node", 8000);
    assert_eq!(e2e.basis, Basis::ProfileMax { runs: 10 });
    // The busiest build used 1.4 CPUs.
    assert_eq!(e2e.cpu, 1400);
}

#[test]
fn a_cpu_budget_holds_back_jobs_that_memory_has_room_for() {
    let mut jobs: Vec<Ran> = (0..10)
        .map(|i| Ran {
            queued_at: NOW + i * 10 * MINUTE,
            started_at: Some(NOW + i * 10 * MINUTE),
            finished_at: Some(NOW + i * 10 * MINUTE + 5 * MINUTE),
            ..build_using(3000)
        })
        .collect();
    // Four builds at once, each using 3 CPUs over its run.
    let burst = NOW + 200 * MINUTE;
    jobs.extend((0..4).map(|_| Ran {
        queued_at: burst,
        started_at: Some(burst),
        finished_at: Some(burst + 5 * MINUTE),
        ..build_using(3000)
    }));
    let slots = BTreeMap::from([("node".to_owned(), 4)]);
    let replay = |cpu_budget| {
        simulate(
            &config(),
            Some(33 * G),
            cpu_budget,
            &jobs,
            Policy::Measured,
            &slots,
        )
    };
    let unbounded = replay(None);
    let bounded = replay(Some(8000));
    assert_eq!(unbounded.most_running, 4);
    // 2 × 3 CPUs fit 8; a third waits.
    assert_eq!(bounded.most_running, 2);
    assert!(bounded.cpu_held > 0, "the CPU budget held a job back");
    assert_eq!(bounded.worst_cpu, 6000);
    assert_eq!(bounded.never_started, 0);
}

#[test]
fn a_job_of_one_name_in_two_pools_is_predicted_from_its_runs_in_each() {
    let mut predictor = predictor();
    // 100 light runs and 2 heavy ones of the same workflow and job name, in two pools' profiles.
    for _ in 0..100 {
        let light = Ran {
            cpu_average: Some(100),
            ..run("halyard/storefront", "test", "light", NOW, 1, 200 * M)
        };
        predictor.learn(&light, "light", 7 * G, NOW);
    }
    for _ in 0..2 {
        let heavy = Ran {
            cpu_average: Some(1800),
            ..run("halyard/storefront", "test", "node", NOW, 5, 3 * G)
        };
        predictor.learn(&heavy, "node", 7 * G, NOW);
    }
    let heavy = predictor.predict(&key("test"), "node", 7 * G, NOW + MINUTE, Copies::Alone);
    assert!(
        heavy.memory >= 3 * G,
        "the heavy pool's job keeps its 3G, not the light runs' 200M: {heavy:?}"
    );
    let cpu = predictor.predict_cpu(&key("test"), "node", 8000);
    assert!(
        cpu.cpu >= 1800,
        "the heavy pool's job keeps its CPU: {cpu:?}"
    );
    let light = predictor.predict(&key("test"), "light", 7 * G, NOW + MINUTE, Copies::Alone);
    assert!(
        light.memory < G,
        "the light pool's job goes by its own runs: {light:?}"
    );
}
