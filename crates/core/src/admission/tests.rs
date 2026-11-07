//! Predictions and replays over synthetic histories: two profiles of a 33G slice, jobs of fictional repositories.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use super::{
    Arrivals, Barriers, Basis, Copies, CreditRoom, HIGH_WATER, JobPeak, Key, Policy, Predictor,
    Replay, Replaying, UNPREDICTABLE_FOR, simulate,
};
use crate::config::Config;
use crate::github::QueuedJob;
use crate::scheduler::{self, Budget, Decision, Drained, Pool, Profile, Repository, Slot, Waits};
use crate::sizing::{History, Measure, Ran};

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
        slot_waited: 0,
        unseen: 0,
        cpu_periods: None,
        cpu_throttled: None,
        hit_cap: Some(false),
        own_cap: Some(false),
        measure: Measure::WithoutPageCache,
        succeeded: true,
        failed: false,
        run_id: queued,
        run_attempt: 1,
        branch: Some("main".to_owned()),
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
    // The profile's p95 is 2G, but e2e reached 5G: 5.75G. The build counts its last 20 runs.
    let e2e = predictor.predict(&key("e2e"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(e2e.basis, Basis::Profile { runs: 22 });
    assert_eq!(e2e.memory, 5888 * M);
    // One light run may not be the job's heaviest: 2G + 15%, rounded up to 2.5G.
    let lint = predictor.predict(&key("lint"), "node", 7 * G, NOW, Copies::Alone);
    assert_eq!(lint.basis, Basis::Profile { runs: 22 });
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
    assert_eq!(prediction.basis, Basis::ProfileMax { runs: 21 });
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
        own_cap: Some(true),
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
            runs: 20,
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
            runs: 20,
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
        own_cap: None,
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
    let caps = simulate(
        &config(),
        Some(33 * G),
        None,
        &jobs,
        Policy::Caps,
        &slots,
        Barriers::Off,
    );
    let measured = simulate(
        &config(),
        Some(33 * G),
        None,
        &jobs,
        Policy::Measured,
        &slots,
        Barriers::Off,
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
        Barriers::Off,
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
        Barriers::Off,
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
        Barriers::Off,
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
        Barriers::Off,
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
            Barriers::Off,
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

/// A node job every 5 minutes running 8, so two of them always overlap in a 16G slice, and at minute 3 a jvm job of
/// 9G, which fits beside none of them. Run times are this test's own.
fn a_stream_of_node_jobs_and_a_large_jvm_one() -> Vec<Ran> {
    let mut jobs: Vec<Ran> = (0..24)
        .map(|i| {
            run(
                "halyard/storefront",
                "site",
                "node",
                NOW + i * 5 * MINUTE,
                8,
                G,
            )
        })
        .collect();
    jobs.insert(
        1,
        run("halyard/ledger", "check", "jvm", NOW + 3 * MINUTE, 4, G),
    );
    jobs
}

fn replayed(jobs: &[Ran], memory_max: u64, barriers: Barriers) -> Replay {
    simulate(
        &config(),
        Some(memory_max),
        None,
        jobs,
        Policy::Caps,
        &BTreeMap::new(),
        barriers,
    )
}

fn pool(repository: &str, profile: &str) -> Pool {
    Pool {
        repository: repository.to_owned(),
        profile: profile.to_owned(),
    }
}

#[test]
fn a_replay_starts_a_starved_job_where_younger_jobs_would_take_all_the_room_before_it() {
    let jobs = a_stream_of_node_jobs_and_a_large_jvm_one();
    let without = replayed(&jobs, 16 * G, Barriers::Off);
    let with = replayed(&jobs, 16 * G, Barriers::On);
    let ledger = pool("halyard/ledger", "jvm");
    let storefront = pool("halyard/storefront", "node");
    let waited = |replay: &Replay| replay.pools[&ledger].waits[0];
    // Past the whole stream without it; with it, the 10 minutes, then what ran by then ending.
    assert!(waited(&without) > 100 * 60, "{}s", waited(&without));
    assert!(waited(&with) <= (10 + 8) * 60, "{}s", waited(&with));
    assert_eq!(with.jobs, without.jobs);
    assert_eq!(with.never_started, 0);
    // What it costs: node jobs wait out the barrier and the jvm job.
    let total = |replay: &Replay| replay.pools[&storefront].waits.iter().sum::<u64>();
    assert!(total(&with) > total(&without), "the node jobs pay for it");
    assert!(with.barrier_for > 0, "a barrier stood");
    assert_eq!(without.barrier_for, 0);
    assert!(with.withheld > 0, "and held free memory back");
}

/// A replay that moves only on arrivals and ends never sees a quiet queue reach the limit.
#[test]
fn a_replay_puts_up_a_barrier_at_the_limit_with_no_job_arriving_then() {
    let jobs = [
        run("halyard/storefront", "site", "node", NOW, 18, G),
        run("halyard/ledger", "check", "jvm", NOW + MINUTE, 1, G),
    ];
    let replay = replayed(&jobs, 12 * G, Barriers::On);
    // It waits from minute 1, holds from minute 11, and the node job ends at minute 18.
    assert_eq!(replay.barrier_for, 7 * MINUTE);
    assert_eq!(
        replay.pools[&pool("halyard/ledger", "jvm")].waits,
        [17 * 60]
    );
}

#[test]
fn a_replay_keeps_no_barrier_for_a_job_no_empty_slice_holds() {
    let jobs = [
        run("halyard/ledger", "check", "jvm", NOW, 1, G),
        run(
            "halyard/storefront",
            "site",
            "node",
            NOW + 10 * MINUTE,
            1,
            G,
        ),
    ];
    let replay = replayed(&jobs, 8 * G, Barriers::On);
    assert_eq!(replay.barrier_for, 0);
    assert_eq!(
        replay.pools[&pool("halyard/ledger", "jvm")].never_started,
        1
    );
    assert_eq!(replay.pools[&pool("halyard/storefront", "node")].waits, [0]);
}

/// Equal queue times are the rule for one workflow's jobs: the replay serves them in the planner's order, the IDs'.
#[test]
fn a_replay_serves_jobs_of_one_moment_in_the_order_it_was_given() {
    let jobs = [
        run("halyard/storefront", "first", "node", NOW, 5, G),
        run("halyard/ledger", "second", "jvm", NOW, 5, G),
    ];
    let replay = replayed(&jobs, 10 * G, Barriers::Off);
    assert_eq!(replay.pools[&pool("halyard/storefront", "node")].waits, [0]);
    assert_eq!(replay.pools[&pool("halyard/ledger", "jvm")].waits, [5 * 60]);
}

/// The busy share reports budget-time, not a count: what ran over all there was.
#[test]
fn a_replay_counts_what_ran_over_the_budgets_time() {
    let jobs = [run("halyard/storefront", "site", "node", NOW, 10, G)];
    // Beside the mirror's 512M.
    let replay = replayed(&jobs, 14 * G + 512 * M, Barriers::On);
    assert_eq!(
        replay.charged * 2,
        replay.capacity,
        "7G of 14G the whole time"
    );
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

mod causal;
mod recent;

fn minutes(minutes: u64) -> Duration {
    Duration::from_secs(minutes * 60)
}

/// Learned in the order given, each run ending when it says.
fn learned(runs: &[Ran]) -> Predictor {
    let mut predictor = predictor();
    for run in runs {
        predictor.learn(run, &run.profile, 7 * G, run.finished_at.unwrap());
    }
    predictor
}

fn site(queued: u64, minutes: u64) -> Ran {
    run("halyard/storefront", "site", "node", queued, minutes, G)
}

/// A job's estimate is what a backfill stakes the owner's room on: the p95 errs long, and two runs are no estimate.
#[test]
fn a_jobs_usual_run_time_is_the_p95_of_its_last_successful_runs_from_three_of_them() {
    let key = key_of("site");
    assert_eq!(predictor().usual(&key, "node"), None, "an empty history");
    let two: Vec<Ran> = (0..2).map(|i| site(NOW + i * MINUTE, 4)).collect();
    assert_eq!(learned(&two).usual(&key, "node"), None);
    let three: Vec<Ran> = [2, 9, 4]
        .iter()
        .zip(0..)
        .map(|(minutes, i)| site(NOW + i * MINUTE, *minutes))
        .collect();
    let usual = learned(&three).usual(&key, "node");
    assert_eq!(usual, Some(minutes(9)), "of few runs, the longest");
    // 20 runs of 5 minutes push the 9 out of the window, an outlier of one in 20 stays out of the p95.
    let mut many = three;
    many.extend((0..19).map(|i| site(NOW + (10 + i) * MINUTE, 5)));
    many.push(site(NOW + 40 * MINUTE, 50));
    assert_eq!(learned(&many).usual(&key, "node"), Some(minutes(5)));
}

fn key_of(name: &str) -> Key {
    Key {
        repository: "halyard/storefront".to_owned(),
        workflow: Some("CI".to_owned()),
        name: name.to_owned(),
    }
}

/// A failed or cancelled run stopped anywhere: its time says nothing of how long the next one takes.
#[test]
fn only_successful_runs_teach_a_usual_run_time() {
    let failed = |i| Ran {
        succeeded: false,
        failed: true,
        ..site(NOW + i * MINUTE, 1)
    };
    let runs: Vec<Ran> = (0..3).map(failed).collect();
    assert_eq!(learned(&runs).usual(&key_of("site"), "node"), None);
    let mut predictor = predictor();
    for i in 0..3 {
        let cancelled = Ran {
            succeeded: false,
            ..site(NOW + i * MINUTE, 1)
        };
        predictor.mark(&cancelled, "node", 7 * G, NOW + i * MINUTE);
    }
    assert_eq!(predictor.usual(&key_of("site"), "node"), None);
    let mut runs: Vec<Ran> = (0..3).map(|i| site(NOW + i * MINUTE, 10)).collect();
    runs.extend((3..6).map(failed));
    assert_eq!(
        learned(&runs).usual(&key_of("site"), "node"),
        Some(minutes(10)),
        "the short failures do not pull it down"
    );
}

/// No finish recorded is no run time: read as 0, it would let any job past a barrier.
#[test]
fn a_successful_run_with_no_finish_teaches_no_usual_run_time() {
    let mut predictor = predictor();
    for i in 0..3 {
        let unfinished = Ran {
            finished_at: None,
            ..site(NOW + i * MINUTE, 1)
        };
        predictor.learn(&unfinished, "node", 7 * G, NOW + i * MINUTE);
    }
    assert_eq!(predictor.usual(&key_of("site"), "node"), None);
}

/// A slot's runner is what a plan sees end, so its start counts; a warm runner's wait for the job does not.
#[test]
fn a_run_keeps_its_runner_from_the_runners_start_or_from_queueing_on_a_warm_one() {
    let fresh = |i| Ran {
        started_at: Some(NOW + i * MINUTE + 30_000),
        runner_started_at: NOW + i * MINUTE,
        ..site(NOW + i * MINUTE, 5)
    };
    let runs: Vec<Ran> = (0..3).map(fresh).collect();
    let usual = learned(&runs).usual(&key_of("site"), "node");
    assert_eq!(usual, Some(minutes(5)), "from the container's start");
    let warm = |i| Ran {
        runner_started_at: NOW + i * MINUTE - 3_600_000,
        ..site(NOW + i * MINUTE, 5)
    };
    let runs: Vec<Ran> = (0..3).map(warm).collect();
    let usual = learned(&runs).usual(&key_of("site"), "node");
    assert_eq!(usual, Some(minutes(5)), "not the hour it stood idle");
}

/// GitHub may give a pool's runner any job its labels match, so a 2-minute job must not speak for a 24-minute one.
#[test]
fn a_pool_goes_by_its_longest_queued_job_and_by_none_if_one_has_no_estimate() {
    let lint = |i| run("halyard/storefront", "lint", "node", NOW + i * MINUTE, 2, G);
    let mut runs: Vec<Ran> = (0..3).map(lint).collect();
    runs.extend((0..3).map(|i| site(NOW + i * MINUTE, 24)));
    let predictor = learned(&runs);
    let keys = |names: &[&str]| names.iter().map(|name| key_of(name)).collect::<Vec<_>>();
    let longest = predictor.longest_usual(keys(&["lint", "site"]), "node");
    assert_eq!(longest, Some(minutes(24)));
    let unknown = predictor.longest_usual(keys(&["lint", "never seen"]), "node");
    assert_eq!(unknown, None);
    assert_eq!(predictor.longest_usual(keys(&[]), "node"), None);
    let other = predictor.longest_usual(keys(&["lint"]), "jvm");
    assert_eq!(other, None, "its runs in another profile say nothing");
}

/// What a replay with needs shows: a job held back holds back what follows it in its run.
fn after_needs(barriers: Barriers, known: Option<&History>) -> Replaying<'_> {
    Replaying {
        barriers,
        arrivals: Arrivals::AfterNeeds,
        known,
        trace: None,
        most_credit: 0,
        host_hold: u64::MAX,
        credit_room: CreditRoom::Counted,
    }
}

fn in_run(run: Ran, run_id: u64, run_attempt: u32) -> Ran {
    Ran {
        run_id,
        run_attempt,
        ..run
    }
}

fn replay_of<'known>(jobs: &[Ran], memory_max: u64, how: impl Into<Replaying<'known>>) -> Replay {
    simulate(
        &config(),
        Some(memory_max),
        None,
        jobs,
        Policy::Caps,
        &BTreeMap::new(),
        how,
    )
}

/// A run whose second job queued 30 s after its first ended, in a slice that holds one node runner: the first waits
/// out another's 5 minutes, so the second arrives that much later, and waits for nothing.
#[test]
fn a_job_that_followed_another_arrives_as_long_after_its_replayed_end() {
    let jobs = [
        site(NOW, 5),
        in_run(site(NOW + MINUTE, 4), 7, 1),
        in_run(site(NOW + 5 * MINUTE + 30_000, 2), 7, 1),
    ];
    let replay = replay_of(&jobs, 8 * G, after_needs(Barriers::Off, None));
    // The second starts at minute 5 and ends at 9; the third arrives at 9m30s.
    assert_eq!(
        replay.pools[&pool("halyard/storefront", "node")].waits,
        [0, 240, 0]
    );
    let of_run_7 = replay.runs.iter().map(|run| run.took).max();
    assert_eq!(of_run_7, Some(10 * 60 + 30), "from minute 1 to 11m30s");
    let as_queued = replay_of(&jobs, 8 * G, Barriers::Off);
    assert_eq!(
        as_queued.pools[&pool("halyard/storefront", "node")].waits,
        [0, 240, 210]
    );
}

/// A job that follows a delayed one queues behind what arrived meanwhile, whatever its ID in the history says: run 7's
/// second job arrives at 14m30s, after a job of the history's minute 6, though it queued before that one.
#[test]
fn a_follower_that_arrives_late_queues_behind_what_arrived_before_it() {
    let jobs = [
        in_run(site(NOW, 10), 5, 1),
        in_run(site(NOW + MINUTE, 4), 7, 1),
        in_run(site(NOW + 2 * MINUTE, 2), 6, 1),
        in_run(site(NOW + 5 * MINUTE + 30_000, 2), 7, 1),
        in_run(site(NOW + 6 * MINUTE, 1), 8, 1),
    ];
    let replay = replay_of(&jobs, 8 * G, after_needs(Barriers::Off, None));
    // One at a time: minutes 0, 10, 14; then the job of minute 6 at 16, and the follower at 17.
    assert_eq!(
        replay.pools[&pool("halyard/storefront", "node")].waits,
        [0, 540, 720, 600, 150]
    );
}

/// A re-run comes back hours later: it follows nothing of the attempt before it.
#[test]
fn a_rerun_follows_nothing_of_the_attempt_before_it() {
    let jobs = [
        in_run(site(NOW, 5), 7, 1),
        in_run(site(NOW + 60 * MINUTE, 5), 7, 2),
    ];
    let replay = replay_of(&jobs, 16 * G, after_needs(Barriers::Off, None));
    assert_eq!(replay.runs.len(), 2);
    assert!(
        replay.runs.iter().all(|run| run.took == 5 * 60),
        "each attempt took its own 5 minutes: {:?}",
        replay.runs
    );
}

#[test]
fn a_job_that_follows_one_never_started_counts_as_never_started() {
    let jobs = [
        in_run(run("halyard/storefront", "check", "jvm", NOW, 5, G), 7, 1),
        in_run(site(NOW + 6 * MINUTE, 1), 7, 1),
    ];
    let replay = replay_of(&jobs, 8 * G, after_needs(Barriers::Off, None));
    assert_eq!(
        replay.never_started, 2,
        "9G fits no 8G slice, so neither ran"
    );
}

/// `CONFIG` with `light` 4 × 1G, for jobs that fit behind a barrier.
fn with_light() -> Config {
    let light = r#"
[profiles.light]
image_stage = "node"
cpus = 1
memory = "1G"
slots = 4

[[repositories]]
name = "halyard/lint"
profile = "light"
labels = ["light"]
"#;
    Config::parse(&format!("{CONFIG}{light}")).unwrap()
}

/// With `with_light()` and 17G, 16.5G past the mirror: two node runners of 7G end at minute 20, so a 9G jvm job queued at
/// minute 1 owns a barrier from minute 11 to then, and 1-minute lint jobs of 1G arrive behind it. `known` says how long
/// each job usually runs: 20, 4 and 1 minutes, and 30 for `report`. Run times are this test's own.
fn a_barrier_with_room_behind_it() -> (Vec<Ran>, History) {
    let check = |queued, minutes| run("halyard/ledger", "check", "jvm", queued, minutes, G);
    let lint = |queued, minutes| run("halyard/lint", "lint", "light", queued, minutes, G / 2);
    let report = |queued, minutes| run("halyard/lint", "report", "light", queued, minutes, G / 2);
    let before = NOW - 600 * MINUTE;
    let known = (0..3)
        .flat_map(|i| {
            let at = before + i * 40 * MINUTE;
            [site(at, 20), check(at, 4), lint(at, 1), report(at, 30)]
        })
        .collect();
    let mut jobs = vec![
        site(NOW, 20),
        site(NOW + 10_000, 20),
        check(NOW + MINUTE, 4),
    ];
    jobs.extend((0..3).map(|i| lint(NOW + (12 + 2 * i) * MINUTE, 1)));
    let history = History {
        jobs: known,
        ..History::default()
    };
    (jobs, history)
}

/// An owner that needs both node runners gone, the first of which ends at minute 14 and lets a node job older than the
/// owner into its slot. A lint job queued half a minute before that would run a minute, past that start.
fn a_barrier_behind_an_older_job() -> (Vec<Ran>, History) {
    let page = |queued, minutes| run("halyard/storefront", "page", "node", queued, minutes, G);
    let lint = |queued, minutes| run("halyard/lint", "lint", "light", queued, minutes, G / 2);
    let report = |queued, minutes| run("halyard/lint", "report", "light", queued, minutes, G / 2);
    let check = |queued, minutes| run("halyard/ledger", "check", "jvm", queued, minutes, G);
    let (_, mut known) = a_barrier_with_room_behind_it();
    known
        .jobs
        .extend((0..3).map(|i| page(NOW - 600 * MINUTE + i * 40 * MINUTE, 14)));
    let jobs = vec![
        page(NOW, 14),
        site(NOW + 1000, 20),
        report(NOW + 2000, 30),
        site(NOW + MINUTE / 2, 20),
        check(NOW + MINUTE, 4),
        lint(NOW + 13 * MINUTE + MINUTE / 2, 1),
    ];
    (jobs, known)
}

/// `jobs` replayed with `with_light()` in 17G, `known` known from the start.
fn replay_with_light(jobs: &[Ran], known: &History, barriers: Barriers) -> Replay {
    let how = Replaying {
        barriers,
        arrivals: Arrivals::AsQueued,
        known: Some(known),
        trace: None,
        most_credit: 0,
        host_hold: u64::MAX,
        credit_room: CreditRoom::Counted,
    };
    simulate(
        &with_light(),
        Some(17 * G),
        None,
        jobs,
        Policy::Caps,
        &BTreeMap::new(),
        how,
    )
}

#[test]
fn a_replay_starts_behind_a_barrier_what_a_strict_one_holds_and_the_owner_starts_all_the_same() {
    let (jobs, known) = a_barrier_with_room_behind_it();
    let strict = replay_with_light(&jobs, &known, Barriers::Strict);
    let backfill = replay_with_light(&jobs, &known, Barriers::On);
    let ledger = pool("halyard/ledger", "jvm");
    let lint = pool("halyard/lint", "light");
    assert_eq!(strict.backfilled.jobs, 0);
    assert_eq!(backfill.backfilled.jobs, 3, "the lint jobs pass");
    assert_eq!(backfill.backfilled.overran, 0, "each within its usual time");
    assert_eq!(strict.pools[&ledger].waits, [19 * 60]);
    assert_eq!(
        backfill.pools[&ledger].waits, strict.pools[&ledger].waits,
        "the owner starts when it would"
    );
    assert_eq!(backfill.pools[&lint].waits, [0, 0, 0]);
}

#[test]
fn a_replay_counts_the_jobs_that_ran_past_the_usual_time_they_passed_by() {
    let (mut jobs, known) = a_barrier_with_room_behind_it();
    let overrun = jobs.last_mut().unwrap();
    overrun.finished_at = overrun.finished_at.map(|end| end + 3 * MINUTE);
    let replay = replay_with_light(&jobs, &known, Barriers::On);
    assert_eq!(replay.backfilled.overran, 1);
    assert_eq!(replay.backfilled.most_overrun, 3 * 60);
}

/// Thirteen 11G jobs queued at minute 9 in three slots of a 42G slice that a stream fills: a 7G job every 80 s for three
/// hours, each running 8 minutes, six of which take all of it. One end leaves too little, and the next of the stream takes
/// it. Each large job runs 10 minutes. Run times are this test's own: 0096's replays assumed 5, 10 or 20 for a large job.
fn thirteen_large_jobs_in_a_stream() -> (Config, Vec<Ran>, History) {
    let config = r#"
[host]
slice = "runnerd.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.heavy]
image_stage = "jvm"
cpus = 8
memory = "11G"
slots = 3

[profiles.mid]
image_stage = "jvm"
cpus = 8
memory = "7G"
slots = 6

[[repositories]]
name = "halyard/ledger"
profile = "heavy"
labels = ["heavy"]

[[repositories]]
name = "halyard/storefront"
profile = "mid"
labels = ["mid"]
"#;
    let heavy = |queued| run("halyard/ledger", "static", "heavy", queued, 10, G);
    let mid = |queued| run("halyard/storefront", "check", "mid", queued, 8, G);
    let known = (0..3)
        .flat_map(|i| {
            let at = NOW - 600 * MINUTE + i * 40 * MINUTE;
            [heavy(at), mid(at)]
        })
        .collect();
    let mut jobs: Vec<Ran> = (0..135).map(|i| mid(NOW + i * 80_000)).collect();
    jobs.extend((0..13).map(|i| heavy(NOW + 9 * MINUTE + i)));
    jobs.sort_by_key(|run| run.queued_at);
    let history = History {
        jobs: known,
        ..History::default()
    };
    (Config::parse(config).unwrap(), jobs, history)
}

/// A pool's wait starts again with each job. With the first limit for every job, each of the large jobs would start a
/// limit after the last, and the last would wait over two hours; the replay of a flat 600 s says 132 minutes.
#[test]
fn a_pool_of_many_large_jobs_is_not_held_to_one_runner_per_first_limit() {
    let (config, jobs, known) = thirteen_large_jobs_in_a_stream();
    let replay = |barriers| {
        let how = Replaying {
            barriers,
            arrivals: Arrivals::AsQueued,
            known: Some(&known),
            trace: None,
            most_credit: 0,
            host_hold: u64::MAX,
            credit_room: CreditRoom::Counted,
        };
        simulate(
            &config,
            Some(42 * G),
            None,
            &jobs,
            Policy::Caps,
            &BTreeMap::new(),
            how,
        )
    };
    let minutes_waited = |replay: &Replay| -> Vec<u64> {
        let waits = &replay.pools[&pool("halyard/ledger", "heavy")].waits;
        waits.iter().map(|wait| wait / 60).collect()
    };
    assert_eq!(
        minutes_waited(&replay(Barriers::On)),
        [12, 16, 17, 22, 26, 27, 32, 36, 37, 42, 46, 47, 52],
        "the first limit, then what a served limit and the room take"
    );
    let without = minutes_waited(&replay(Barriers::Off));
    assert!(without[0] > 150, "the stream keeps them out: {without:?}");
}

/// A mark that outlived the queue would put the next burst's barrier up early, in front of what may still go by.
#[test]
fn a_replay_goes_back_to_the_first_limit_once_a_pools_queue_emptied() {
    let (mut jobs, known) = a_barrier_with_room_behind_it();
    let later = NOW + 60 * MINUTE;
    jobs.extend([
        site(later, 20),
        site(later + 1000, 20),
        run("halyard/ledger", "check", "jvm", later + MINUTE, 4, G),
        run(
            "halyard/lint",
            "report",
            "light",
            later + 5 * MINUTE,
            30,
            G / 2,
        ),
    ]);
    let replay = replay_with_light(&jobs, &known, Barriers::On);
    assert_eq!(
        replay.pools[&pool("halyard/lint", "light")].waits.last(),
        Some(&0),
        "4 minutes into its wait, the jvm job holds nothing back"
    );
    let replayed: BTreeMap<Pool, Vec<u64>> = replay
        .pools
        .iter()
        .map(|(pool, replayed)| (pool.clone(), replayed.waits.clone()))
        .collect();
    assert_eq!(through_the_planner(&jobs, &known), replayed);
}

/// `jobs` through `scheduler::plan` itself, second by second, as a model of the engine commits it: each pool's waits in
/// the order its jobs started. Under caps, whose runners take their profile's memory; `known` gives every usual run time.
#[allow(
    clippy::too_many_lines,
    reason = "one loop models a tick of the engine; split up, it reads worse"
)]
fn through_the_planner(jobs: &[Ran], known: &History) -> BTreeMap<Pool, Vec<u64>> {
    let config = with_light();
    let predictor = Predictor::read(&config.sizing, known, |_| None);
    let budget = 17 * G - config.mirror_memory();
    let pools: Vec<Pool> = config.repositories.iter().map(Pool::from).collect();
    let mut waits: BTreeMap<Pool, Waits> = BTreeMap::new();
    let mut waited: BTreeMap<Pool, Vec<u64>> = BTreeMap::new();
    let mut queued: Vec<(u64, &Ran)> = Vec::new();
    // Each runner: its pool, when it started and ends, in ms, and how long its job usually runs.
    let mut running: Vec<(Pool, u64, u64, Option<Duration>)> = Vec::new();
    let mut arriving: Vec<(u64, &Ran)> = (0_u64..).zip(jobs).collect();
    arriving.reverse();
    let mut now = NOW;
    while !arriving.is_empty() || !queued.is_empty() || !running.is_empty() {
        running.retain(|(_, _, ends, _)| *ends > now);
        while arriving.last().is_some_and(|(_, run)| run.queued_at <= now) {
            queued.extend(arriving.pop());
        }
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis(now);
        let of_pool = |pool: &Pool, run: &Ran| {
            run.repository == pool.repository && run.profile == pool.profile
        };
        let repositories: Vec<Repository> = pools
            .iter()
            .map(|pool| {
                let matching = queued.iter().filter(|(_, run)| of_pool(pool, run));
                let usual = predictor
                    .longest_usual(matching.clone().map(|(_, run)| Key::of(run)), &pool.profile);
                Repository {
                    name: pool.repository.clone(),
                    profile: pool.profile.clone(),
                    labels: vec![pool.profile.clone()],
                    paused: false,
                    held: None,
                    queued: matching
                        .map(|(id, _)| QueuedJob {
                            id: *id,
                            labels: vec![pool.profile.clone()],
                            ..QueuedJob::default()
                        })
                        .collect(),
                    runner_memory: None,
                    runner_cpu: None,
                    waits: waits.get(pool).copied().unwrap_or_default(),
                    usual,
                    warm: 0,
                }
            })
            .collect();
        let mut used = 0;
        let profiles: BTreeMap<String, Profile> = config
            .profiles
            .iter()
            .map(|(name, limits)| {
                let mut slots: Vec<Slot> = running
                    .iter()
                    .filter(|(pool, ..)| pool.profile == *name)
                    .map(|(pool, started, _, usual)| {
                        used += limits.memory.0;
                        Slot::Runner(scheduler::RunnerSlot {
                            repository: pool.repository.clone(),
                            busy: true,
                            started_at: SystemTime::UNIX_EPOCH + Duration::from_millis(*started),
                            memory: limits.memory.0,
                            cpu: 0,
                            usual: *usual,
                        })
                    })
                    .collect();
                slots.resize(limits.slots as usize, Slot::Free);
                let profile = Profile {
                    memory: limits.memory.0,
                    cpus: 0,
                    preparing: None,
                    upkeep_due: false,
                    count: limits.slots as usize,
                    ran: Vec::new(),
                    slots,
                };
                (name.clone(), profile)
            })
            .collect();
        let plan = scheduler::plan(
            &repositories,
            &profiles,
            Budget {
                memory: Some(i64::try_from(budget - used).unwrap()),
                drained: Drained {
                    memory: Some(i64::try_from(budget).unwrap()),
                    ..Drained::default()
                },
                ..Budget::default()
            },
            at,
        );
        for decision in &plan.decisions {
            let Decision::Start {
                repository, slot, ..
            } = decision
            else {
                continue;
            };
            let pool = Pool {
                repository: repository.clone(),
                profile: slot.profile.clone(),
            };
            let oldest = queued
                .iter()
                .position(|(_, run)| of_pool(&pool, run))
                .unwrap();
            let (_, run) = queued.remove(oldest);
            let ran_for = run.finished_at.unwrap() - run.started_at.unwrap();
            let usual = predictor.usual(&Key::of(run), &pool.profile);
            if let Some(waits) = waits.get_mut(&pool) {
                waits.started();
            }
            waited
                .entry(pool.clone())
                .or_default()
                .push((now - run.queued_at) / 1000);
            running.push((pool, now, now + ran_for, usual));
        }
        for demand in &plan.held {
            waits
                .entry(demand.pool.clone())
                .or_default()
                .note(demand.scarce, demand.job, at);
        }
        for pool in &pools {
            let waits = waits.entry(pool.clone()).or_default();
            if !queued.iter().any(|(_, run)| of_pool(pool, run)) {
                waits.emptied();
            }
            if let Some(barrier) = plan.barrier {
                waits.own(barrier.job);
            }
        }
        now += 1000;
    }
    waited
}

/// `simulate` promises what `plan` does: a replay that drifted from the rule would show figures the daemon never gives.
#[test]
fn the_replay_and_the_planner_start_a_shared_fixture_at_the_same_moments() {
    let (mut jobs, known) = a_barrier_with_room_behind_it();
    // Too long to pass, and while it is queued its pool goes by it: the next lint job waits with it.
    jobs.insert(
        4,
        run(
            "halyard/lint",
            "report",
            "light",
            NOW + 13 * MINUTE,
            30,
            G / 2,
        ),
    );
    let replay = replay_with_light(&jobs, &known, Barriers::On);
    assert_eq!(replay.backfilled.jobs, 1, "the first lint job alone passed");
    let replayed: BTreeMap<Pool, Vec<u64>> = replay
        .pools
        .iter()
        .map(|(pool, replayed)| (pool.clone(), replayed.waits.clone()))
        .collect();
    assert_eq!(through_the_planner(&jobs, &known), replayed);
    let (jobs, known) = a_barrier_behind_an_older_job();
    let replay = replay_with_light(&jobs, &known, Barriers::On);
    assert_eq!(
        replay.backfilled.jobs, 0,
        "the lint job waits for the older one"
    );
    let replayed: BTreeMap<Pool, Vec<u64>> = replay
        .pools
        .iter()
        .map(|(pool, replayed)| (pool.clone(), replayed.waits.clone()))
        .collect();
    assert_eq!(through_the_planner(&jobs, &known), replayed);
    // A served pool: job two's short limit is up at minute 22, so what would take its room waits; with the first limit
    // the page and report jobs start at minute 23 and it waits out the report's 30 minutes.
    let (mut jobs, mut known) = a_barrier_with_room_behind_it();
    let page = |queued, minutes| run("halyard/storefront", "page", "node", queued, minutes, G);
    let report = |queued, minutes| run("halyard/lint", "report", "light", queued, minutes, G / 2);
    let check = run("halyard/ledger", "check", "jvm", NOW + MINUTE + 1000, 4, G);
    jobs.insert(3, check);
    jobs.extend([page(NOW + 23 * MINUTE, 6), report(NOW + 23 * MINUTE, 30)]);
    known.jobs.extend((0..3).flat_map(|i| {
        let at = NOW - 600 * MINUTE + i * 40 * MINUTE;
        [page(at, 6), report(at, 30)]
    }));
    let replay = replay_with_light(&jobs, &known, Barriers::On);
    let ledger = pool("halyard/ledger", "jvm");
    assert_eq!(replay.pools[&ledger].waits, [19 * 60, 23 * 60 - 1]);
    let replayed: BTreeMap<Pool, Vec<u64>> = replay
        .pools
        .iter()
        .map(|(pool, replayed)| (pool.clone(), replayed.waits.clone()))
        .collect();
    assert_eq!(through_the_planner(&jobs, &known), replayed);
}
