//! Predictions and replays over synthetic histories: two profiles of a 33G slice, jobs of fictional repositories.

use std::collections::BTreeMap;

use super::{Basis, HIGH_WATER, Key, Policy, Predictor, UNPREDICTABLE_FOR, simulate};
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
        memory_peak: peak,
        cpu_peak: 3000,
        queued_at: queued,
        started_at: Some(queued),
        finished_at: Some(queued + minutes * MINUTE),
        oom_killed: false,
        reserved: None,
        hit_cap: Some(false),
        measure: Measure::Unreclaimable,
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
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW);
    assert_eq!(prediction.basis, Basis::Job { runs: 20 });
    assert_eq!(prediction.memory, 2304 * M);
}

#[test]
fn a_job_with_few_runs_takes_its_profiles_p95_or_its_own_largest_peak_whichever_is_more() {
    let mut predictor = predictor();
    for _ in 0..60 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    let e2e = run("halyard/storefront", "e2e", "node", NOW, 20, 5 * G);
    for _ in 0..3 {
        predictor.learn(&e2e, "node", 7 * G, NOW);
    }
    // The profile's p95 is 2G, but e2e reached 5G: 5.75G.
    let prediction = predictor.predict(&key("e2e"), "node", 7 * G, NOW);
    assert_eq!(prediction.basis, Basis::Profile { runs: 63 });
    assert_eq!(prediction.memory, 5888 * M);
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
    let prediction = predictor.predict(&key("lint"), "node", 7 * G, NOW);
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
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE);
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
            .predict(&key("build"), "node", 7 * G, NOW + UNPREDICTABLE_FOR)
            .basis,
        Basis::Job { runs: 20 }
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
    predictor.mark(&killed, 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE);
    assert_eq!(
        prediction.basis,
        Basis::Unpredictable {
            until: (NOW + UNPREDICTABLE_FOR) / 1000
        }
    );
    // Its peak of 0 did not join the job's runs.
    assert_eq!(
        predictor
            .predict(&key("build"), "node", 7 * G, NOW + UNPREDICTABLE_FOR)
            .basis,
        Basis::Job { runs: 20 }
    );
}

#[test]
fn without_enough_runs_of_the_job_or_its_profile_a_job_is_admitted_at_its_cap() {
    let mut predictor = predictor();
    for _ in 0..3 {
        predictor.learn(&build(G), "node", 7 * G, NOW);
    }
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW);
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
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE);
    assert_eq!(
        prediction.basis,
        Basis::Unpredictable {
            until: (NOW + UNPREDICTABLE_FOR) / 1000
        }
    );
    assert_eq!(prediction.memory, 7 * G);
    let later = predictor.predict(&key("build"), "node", 7 * G, NOW + UNPREDICTABLE_FOR);
    assert_eq!(later.basis, Basis::Job { runs: 21 });
}

#[test]
fn a_job_whose_page_cache_fills_its_cap_is_not_put_back_on_its_cap() {
    let mut predictor = predictor();
    for _ in 0..20 {
        predictor.learn(&build(2 * G), "node", 7 * G, NOW);
    }
    // One run came close to its cap and the kernel never ran out of memory for it: it reclaimed page cache.
    predictor.learn(&build(6900 * M), "node", 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE);
    assert_eq!(prediction.basis, Basis::Job { runs: 21 });
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
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE);
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
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE);
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
        predictor.predict(&key("build"), "node", 7 * G, NOW).memory,
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
    let caps = simulate(&config(), Some(33 * G), &jobs, Policy::Caps, &slots);
    let measured = simulate(&config(), Some(33 * G), &jobs, Policy::Measured, &slots);
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
    let measured = simulate(&config(), Some(33 * G), &jobs, Policy::Measured, &slots);
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
    predictor.mark(&cancelled, 7 * G, NOW);
    let prediction = predictor.predict(&key("build"), "node", 7 * G, NOW + MINUTE);
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
        7 * G,
        NOW,
    );
    let prediction = predictor.predict(&key("e2e"), "node", 7 * G, NOW + MINUTE);
    assert_eq!(prediction.basis, Basis::Cap);
    assert_eq!(prediction.memory, 7 * G);
}
