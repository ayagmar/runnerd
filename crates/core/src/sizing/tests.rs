//! Advice from synthetic histories: two profiles of a 33G slice, jobs of fictional repositories.

use super::{Held, History, Measure, Ran, Window, advise};
use crate::api::{Advice, ProfileSizing, Sizing};
use crate::config::{ByteSize, Config, resize_profile};

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;
const HOUR: u64 = 3600 * 1000;
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

fn job(repository: &str, name: &str, profile: &str, peak: u64, wait_secs: u64) -> Ran {
    let cap = if profile == "jvm" { 9 * G } else { 7 * G };
    Ran {
        repository: repository.to_owned(),
        workflow: Some("CI".to_owned()),
        name: name.to_owned(),
        profile: profile.to_owned(),
        memory_cap: Some(cap),
        memory_peak: peak,
        cpu_peak: 3000,
        cpu_average: Some(1000),
        queued_at: NOW - 2 * HOUR,
        started_at: Some(NOW - 2 * HOUR + wait_secs * 1000),
        finished_at: Some(NOW - HOUR),
        oom_killed: false,
        reserved: None,
        hit_cap: None,
        measure: Measure::WithoutPageCache,
        succeeded: true,
    }
}

/// `count` storefront builds peaking between `low` and `high`, spread evenly, each waiting `wait_secs`.
fn builds(count: u64, low: u64, high: u64, wait_secs: u64) -> Vec<Ran> {
    (0..count)
        .map(|i| {
            let peak = low + (high - low) * i / (count - 1).max(1);
            job("halyard/storefront", "build", "node", peak, wait_secs)
        })
        .collect()
}

fn sizing(history: &History) -> Sizing {
    advise(
        &config(),
        Some(33 * G),
        history,
        Window {
            since: NOW - 30 * 24 * HOUR,
            now: NOW,
            sample_interval: 2,
        },
    )
}

fn profile<'sizing>(sizing: &'sizing Sizing, name: &str) -> &'sizing ProfileSizing {
    sizing.profiles.iter().find(|p| p.name == name).unwrap()
}

#[test]
fn a_cap_far_above_the_peaks_is_lowered_and_waiting_jobs_get_the_slots_that_fit() {
    // p99 2.1G, + 15% = 2.415G, rounded up to 2.5G; 2 × 7G = 14G holds 5 of them.
    let history = History {
        jobs: builds(100, 1200 * M, 2150 * M, 300),
        cancelled: Vec::new(),
        runners: Vec::new(),
    };
    let node = profile(&sizing(&history), "node").clone();
    assert_eq!(node.peaks.jobs, 100);
    assert_eq!(
        node.advice,
        Advice::Lower {
            memory: 2560 * M,
            slots: 5
        }
    );
}

#[test]
fn a_lowered_cap_keeps_its_slots_when_no_job_waited_for_one() {
    let history = History {
        jobs: builds(20, 1000 * M, 2000 * M, 5),
        cancelled: Vec::new(),
        runners: Vec::new(),
    };
    // 2000M + 15% = 2300M, rounded up to 2304M.
    assert_eq!(
        profile(&sizing(&history), "node").advice,
        Advice::Lower {
            memory: 2304 * M,
            slots: 2
        }
    );
}

#[test]
fn a_rare_heavy_job_keeps_the_cap_above_its_largest_peak() {
    // 99 light builds and 3 end-to-end runs at 6G: the profile's p99 is light, but lowering to it would kill e2e.
    let mut jobs = builds(99, 1000 * M, 1500 * M, 0);
    jobs.extend((0..3).map(|_| job("halyard/storefront", "e2e", "node", 6 * G, 0)));
    let sizing = sizing(&History {
        jobs,
        cancelled: Vec::new(),
        runners: Vec::new(),
    });
    let node = profile(&sizing, "node");
    // 6G + 15% = 6.9G, rounded up to 7G: the cap stays.
    assert_eq!(node.advice, Advice::Keep { memory: 7 * G });
    let e2e = node.jobs.iter().find(|j| j.name == "e2e").unwrap();
    assert_eq!(e2e.advice, Advice::TooFew { jobs: 3 }, "3 runs are too few");
}

#[test]
fn jobs_that_reach_their_cap_raise_it_within_what_the_slice_holds() {
    // Ledger tests peak at 8.8G of 9G: 10.25G with the margin. The slice has 0.5G spare, so 2 slots of 10.25G
    // (20.5G) do not fit in the 18.5G there is; one does.
    let jobs = (0..12)
        .map(|i| job("halyard/ledger", "test", "jvm", 8 * G + (800 + i) * M, 10))
        .collect();
    let sizing = sizing(&History {
        jobs,
        cancelled: Vec::new(),
        runners: Vec::new(),
    });
    let jvm = profile(&sizing, "jvm");
    assert_eq!(jvm.peaks.at_cap, 12, "every run peaked within 5% of 9G");
    assert_eq!(
        jvm.advice,
        Advice::Raise {
            memory: 10496 * M,
            slots: 1
        }
    );
}

#[test]
fn too_few_jobs_give_no_advice_but_still_show_their_peaks() {
    let sizing = sizing(&History {
        jobs: builds(4, G, 2 * G, 0),
        cancelled: Vec::new(),
        runners: Vec::new(),
    });
    let node = profile(&sizing, "node");
    assert_eq!(node.advice, Advice::TooFew { jobs: 4 });
    assert_eq!(node.peaks.memory.max, 2 * G);
    assert_eq!(profile(&sizing, "jvm").advice, Advice::TooFew { jobs: 0 });
}

#[test]
fn percentiles_are_nearest_rank_over_the_jobs_peaks() {
    // Peaks 1M … 200M: p50 the 100th, p95 the 190th, p99 the 198th.
    let jobs = (1..=200)
        .map(|i| job("halyard/storefront", "build", "node", i * M, i))
        .collect();
    let node = profile(
        &sizing(&History {
            jobs,
            cancelled: Vec::new(),
            runners: Vec::new(),
        }),
        "node",
    )
    .clone();
    assert_eq!(
        (
            node.peaks.memory.p50,
            node.peaks.memory.p95,
            node.peaks.memory.p99,
            node.peaks.memory.max
        ),
        (100 * M, 190 * M, 198 * M, 200 * M)
    );
    assert_eq!((node.wait.p50, node.wait.p95), (100, 190));
}

#[test]
fn busy_is_the_share_of_slot_time_runners_held_since_the_first_runner() {
    // The history starts 10 hours ago; one of jvm's 2 slots was held for 5 hours: 25%.
    let runners = vec![
        Held {
            profile: "jvm".to_owned(),
            started_at: NOW - 10 * HOUR,
            removed_at: Some(NOW - 7 * HOUR),
        },
        Held {
            profile: "jvm".to_owned(),
            started_at: NOW - 2 * HOUR,
            removed_at: None,
        },
    ];
    let sizing = sizing(&History {
        jobs: Vec::new(),
        cancelled: Vec::new(),
        runners,
    });
    assert_eq!(sizing.since, (NOW - 10 * HOUR) / 1000);
    assert_eq!(profile(&sizing, "jvm").busy, 25);
    assert_eq!(profile(&sizing, "node").busy, 0);
}

#[test]
fn each_advised_change_written_into_the_config_still_fits_the_slice() {
    for high in [300 * M, 2 * G, 5 * G, 6800 * M, 8 * G, 12 * G] {
        for wait in [0, 600] {
            let mut jobs = builds(30, 200 * M, high.min(7 * G), wait);
            jobs.extend((0..30).map(|i| job("halyard/ledger", "test", "jvm", high + i * M, wait)));
            let sizing = sizing(&History {
                jobs,
                cancelled: Vec::new(),
                runners: Vec::new(),
            });
            for profile in &sizing.profiles {
                let Some(change) = profile.advice.change() else {
                    continue;
                };
                let text = resize_profile(CONFIG, &profile.name, change).unwrap();
                let (_, checked) = Config::check(&text, |_| Ok(Some(ByteSize(33 * G))));
                assert!(
                    checked.valid,
                    "{}'s advice for peaks up to {high}, waits of {wait}s: {}",
                    profile.name, checked.message
                );
            }
        }
    }
}

#[test]
fn a_job_killed_for_memory_keeps_its_cap_however_low_its_sampled_peaks() {
    // The kills came between samples: every sampled peak is 2G of the 7G cap.
    let mut jobs = builds(20, 2 * G, 2 * G, 0);
    jobs[0].oom_killed = true;
    jobs[0].hit_cap = Some(true);
    let sizing = sizing(&History {
        jobs,
        cancelled: Vec::new(),
        runners: Vec::new(),
    });
    let node = profile(&sizing, "node");
    assert_eq!(node.peaks.at_cap, 1);
    assert!(
        matches!(node.advice, Advice::Keep { .. }),
        "the cap stays: {:?}",
        node.advice
    );
}

#[test]
fn runs_killed_at_an_older_smaller_cap_keep_the_raised_cap() {
    // node was 4G and its builds died at it; it is 7G now. Their peaks at the old cap say they need at least that,
    // not that 4.75G is enough.
    let jobs = (0..20)
        .map(|_| Ran {
            memory_cap: Some(4 * G),
            ..job("halyard/storefront", "build", "node", 4 * G - 50 * M, 0)
        })
        .collect();
    let sizing = sizing(&History {
        jobs,
        cancelled: Vec::new(),
        runners: Vec::new(),
    });
    let node = profile(&sizing, "node");
    assert_eq!(node.peaks.at_cap, 20);
    assert!(
        matches!(node.advice, Advice::Keep { .. }),
        "the cap stays: {:?}",
        node.advice
    );
    assert_eq!(
        node.jobs[0].advice.change(),
        None,
        "nor is the job's lowered"
    );
}

#[test]
fn the_heaviest_runs_of_a_job_with_hundreds_of_runs_keep_the_cap_they_need() {
    // 198 builds at 2G and 2 at 6G: the p99 is 2G, but a 2.5G cap would kill the 6G ones each time they recur.
    let mut jobs = builds(198, 2 * G, 2 * G, 0);
    jobs.extend(builds(2, 6 * G, 6 * G, 0));
    let sizing = sizing(&History {
        jobs,
        cancelled: Vec::new(),
        runners: Vec::new(),
    });
    let node = profile(&sizing, "node");
    assert_eq!(node.peaks.memory.p99, 2 * G, "the p99 is shown as it is");
    // 6G + 15% = 6.9G, rounded up to 7G.
    assert_eq!(node.advice, Advice::Keep { memory: 7 * G });
    assert_eq!(node.jobs[0].advice, Advice::Keep { memory: 7 * G });
}

#[test]
fn jobs_that_need_more_than_the_slice_holds_for_one_slot_ask_for_a_larger_slice() {
    // Ledger tests need 10.5G and the slice is 9G: no number of slots fits, so there is nothing to write.
    let jobs = (0..12)
        .map(|_| job("halyard/ledger", "test", "jvm", 9 * G, 0))
        .collect();
    let sizing = advise(
        &config(),
        Some(9 * G),
        &History {
            jobs,
            cancelled: Vec::new(),
            runners: Vec::new(),
        },
        Window::new(None, NOW / 1000),
    );
    let jvm = profile(&sizing, "jvm");
    assert_eq!(jvm.advice, Advice::SliceTooSmall { memory: 10752 * M });
    assert_eq!(jvm.advice.change(), None);
    assert!(
        sizing.advice_line(jvm).contains("MemoryMax"),
        "the line says the slice has to grow: {}",
        sizing.advice_line(jvm)
    );
}

#[test]
fn a_start_in_the_future_reads_from_now() {
    let window = Window::new(Some(u64::MAX / 10), NOW / 1000);
    assert_eq!(window.since, window.now);
    assert!(
        i64::try_from(window.since).is_ok(),
        "SQLite binds it as an integer"
    );
}

/// The owner's host: `admission = "measured"`, `node` 4 × 5.5G beside `jvm` 2 × 9G and the mirror. Its caps sum to
/// 40.5G, over the 33G slice, which measured admission allows.
fn measured() -> String {
    CONFIG
        .replace(
            "isolation = \"sysbox\"",
            "isolation = \"sysbox\"\nadmission = \"measured\"",
        )
        .replace("memory = \"7G\"\nslots = 2", "memory = \"5.5G\"\nslots = 4")
}

fn measured_sizing(history: &History, memory_max: u64) -> Sizing {
    advise(
        &Config::parse(&measured()).unwrap(),
        Some(memory_max),
        history,
        Window::new(None, NOW / 1000),
    )
}

/// `count` storefront builds at a 5.5G cap peaking at `peak`, each waiting `wait_secs`.
fn capped_builds(count: u64, peak: u64, wait_secs: u64) -> Vec<Ran> {
    (0..count)
        .map(|_| Ran {
            memory_cap: Some(5632 * M),
            ..job("halyard/storefront", "build", "node", peak, wait_secs)
        })
        .collect()
}

#[test]
fn applying_memory_advice_under_measured_admission_keeps_the_profiles_slots() {
    // Builds reach 5.4G of 5.5G: 6.21G with the margin, 6.25G rounded. Four of them at their caps would not fit
    // beside jvm's caps, but measured admission never reserves caps, so the slots stay.
    let sizing = measured_sizing(
        &History {
            jobs: capped_builds(20, 5400 * M, 0),
            cancelled: Vec::new(),
            runners: Vec::new(),
        },
        33 * G,
    );
    let node = profile(&sizing, "node");
    assert_eq!(
        node.advice,
        Advice::Raise {
            memory: 6400 * M,
            slots: 4
        }
    );
    let text = resize_profile(&measured(), "node", node.advice.change().unwrap()).unwrap();
    let (_, checked) = Config::check(&text, |_| Ok(Some(ByteSize(33 * G))));
    assert!(checked.valid, "the raise applies: {}", checked.message);
    assert!(
        !sizing.advice_line(node).contains("slots fit"),
        "the line does not talk of cutting slots: {}",
        sizing.advice_line(node)
    );
}

#[test]
fn under_measured_admission_a_raise_is_refused_only_when_one_runner_and_the_mirror_overflow_the_slice()
 {
    // 6.25G and the 512M mirror need 6.75G: a 6.5G slice holds no runner of it.
    let sizing = measured_sizing(
        &History {
            jobs: capped_builds(20, 5400 * M, 0),
            cancelled: Vec::new(),
            runners: Vec::new(),
        },
        6656 * M,
    );
    assert_eq!(
        profile(&sizing, "node").advice,
        Advice::SliceTooSmall { memory: 6400 * M }
    );
}

#[test]
fn under_measured_admission_waiting_jobs_are_told_about_slots_without_changing_them() {
    // Builds peak at 2G of 5.5G and waited 5 minutes for a slot: lower the cap, keep the slots, and say so.
    let jobs = (0..20)
        .map(|_| Ran {
            memory_cap: Some(5632 * M),
            ..job("halyard/storefront", "build", "node", 2 * G, 300)
        })
        .collect();
    let sizing = measured_sizing(
        &History {
            jobs,
            cancelled: Vec::new(),
            runners: Vec::new(),
        },
        33 * G,
    );
    let node = profile(&sizing, "node");
    assert_eq!(
        node.advice,
        Advice::Lower {
            memory: 2560 * M,
            slots: 4
        }
    );
    let line = sizing.advice_line(node);
    assert!(
        line.contains("slots"),
        "the line suggests more slots: {line}"
    );
}
