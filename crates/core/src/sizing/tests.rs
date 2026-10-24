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
        cpu_cap: Some(8000),
        memory_peak: peak,
        cpu_peak: 6000,
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
    let auto = Sizing {
        auto: true,
        ..sizing.clone()
    };
    let line = auto.advice_line(profile(&auto, "node"));
    assert!(
        line.contains("auto sizing adds one") && !line.contains("by hand"),
        "with auto sizing on, the line says what it does: {line}"
    );
}

mod auto {
    //! Auto sizing from the same histories: what the daemon writes into the config by itself.

    use std::collections::BTreeMap;

    use super::{
        CONFIG, G, HOUR, History, M, NOW, Ran, Window, builds, capped_builds, job, measured,
    };
    use crate::api::{Resize, SizingChange};
    use crate::config::Config;
    use crate::sizing::auto::{Context, changes};

    /// `text` with `[sizing] auto` on and `rules` added.
    fn on(text: &str, rules: &str) -> Config {
        Config::parse(&format!("{text}\n[sizing]\nauto = true\n{rules}")).unwrap()
    }

    fn window() -> Window {
        Window {
            since: NOW - 30 * 24 * HOUR,
            now: NOW,
            sample_interval: 2,
        }
    }

    fn history(jobs: Vec<Ran>) -> History {
        History {
            jobs,
            cancelled: Vec::new(),
            runners: Vec::new(),
        }
    }

    /// The changes due at `NOW` against a slice of `memory_max`, after the last changes `last`, with nothing
    /// running.
    fn due(
        config: &Config,
        history: &History,
        memory_max: u64,
        last: &BTreeMap<String, u64>,
    ) -> Vec<SizingChange> {
        changes(
            config,
            history,
            Context {
                memory_max: Some(memory_max),
                cpus: Some(32_000),
                window: window(),
                last,
                running: &BTreeMap::new(),
            },
        )
    }

    fn resized(changes: &[SizingChange]) -> Vec<(&str, Resize)> {
        changes
            .iter()
            .map(|change| (change.profile.as_str(), change.to))
            .collect()
    }

    #[test]
    fn auto_sizing_lowers_an_oversized_profile_once_it_has_enough_runs() {
        let config = on(CONFIG, "");
        // Peaks up to 2G need 2.25G of node's 7G, but a change moves memory by at most 25%: 5.25G.
        let few = history(builds(9, 1000 * M, 2000 * M, 5));
        assert_eq!(due(&config, &few, 33 * G, &BTreeMap::new()), Vec::new());
        let enough = history(builds(10, 1000 * M, 2000 * M, 5));
        let lowered = due(&config, &enough, 33 * G, &BTreeMap::new());
        assert_eq!(
            resized(&lowered),
            vec![(
                "node",
                Resize {
                    memory: 5376 * M,
                    slots: 2,
                    cpus: None
                }
            )]
        );
        assert_eq!(
            lowered[0].from,
            Resize {
                memory: 7 * G,
                slots: 2,
                cpus: None
            },
            "the change keeps what it replaced, to undo it"
        );
        assert_eq!(lowered[0].at, NOW / 1000);
    }

    #[test]
    fn auto_sizing_is_off_unless_the_config_turns_it_on() {
        let config = Config::parse(CONFIG).unwrap();
        let enough = history(builds(10, 1000 * M, 2000 * M, 5));
        assert_eq!(due(&config, &enough, 33 * G, &BTreeMap::new()), Vec::new());
    }

    #[test]
    fn a_profile_lowered_step_by_step_stops_at_its_min_memory() {
        let config = on(
            &CONFIG.replace("memory = \"7G\"", "memory = \"7G\"\nmin_memory = \"6G\""),
            "max_step = 100",
        );
        let enough = history(builds(10, 1000 * M, 2000 * M, 5));
        assert_eq!(
            resized(&due(&config, &enough, 33 * G, &BTreeMap::new())),
            vec![(
                "node",
                Resize {
                    memory: 6 * G,
                    slots: 2,
                    cpus: None
                }
            )]
        );
    }

    #[test]
    fn waits_the_interval_between_changes() {
        let config = on(CONFIG, "interval = \"12h\"");
        let enough = history(builds(10, 1000 * M, 2000 * M, 5));
        let changed =
            |hours_ago: u64| BTreeMap::from([("node".to_owned(), (NOW - hours_ago * HOUR) / 1000)]);
        assert_eq!(due(&config, &enough, 33 * G, &changed(11)), Vec::new());
        let long = on(CONFIG, "interval = \"60d\"");
        assert_eq!(
            due(&long, &enough, 33 * G, &changed(45 * 24)),
            Vec::new(),
            "an interval longer than the advice's 30 days still holds"
        );
        assert_eq!(
            resized(&due(&config, &enough, 33 * G, &changed(13))),
            vec![(
                "node",
                Resize {
                    memory: 5376 * M,
                    slots: 2,
                    cpus: None
                }
            )]
        );
    }

    /// Ten ledger tests at jvm's 9G cap: they need 10.5G a slot.
    fn at_jvm_cap() -> History {
        history(
            (0..10)
                .map(|_| job("halyard/ledger", "test", "jvm", 9 * G, 5))
                .collect(),
        )
    }

    #[test]
    fn never_raises_past_memory_max() {
        let config = on(CONFIG, "");
        // 24G less node's 14G and the mirror's 0.5G leaves 9.5G: not one slot of 10.5G.
        assert_eq!(
            due(&config, &at_jvm_cap(), 24 * G, &BTreeMap::new()),
            Vec::new()
        );
        // 33G leaves 18.5G: one slot of 10.5G.
        assert_eq!(
            resized(&due(&config, &at_jvm_cap(), 33 * G, &BTreeMap::new())),
            vec![(
                "jvm",
                Resize {
                    memory: 10752 * M,
                    slots: 1,
                    cpus: None
                }
            )]
        );
    }

    #[test]
    fn a_raise_stops_at_the_profiles_max_memory() {
        let config = on(
            &CONFIG.replace("memory = \"9G\"", "memory = \"9G\"\nmax_memory = \"10G\""),
            "",
        );
        assert_eq!(
            resized(&due(&config, &at_jvm_cap(), 36 * G, &BTreeMap::new())),
            vec![(
                "jvm",
                Resize {
                    memory: 10 * G,
                    slots: 2,
                    cpus: None
                }
            )]
        );
    }

    #[test]
    fn a_raise_that_takes_a_slot_away_waits_while_every_slot_runs() {
        let config = on(CONFIG, "");
        let running = BTreeMap::from([("jvm".to_owned(), 2)]);
        let raised = changes(
            &config,
            &at_jvm_cap(),
            Context {
                memory_max: Some(33 * G),
                cpus: Some(32_000),
                window: window(),
                last: &BTreeMap::new(),
                running: &running,
            },
        );
        assert_eq!(raised, Vec::new());
    }

    #[test]
    fn never_lowers_a_profile_a_cancelled_run_was_killed_for_memory_in() {
        let config = on(CONFIG, "");
        let mut killed = history(builds(10, 1000 * M, 2000 * M, 5));
        killed.cancelled.push(Ran {
            oom_killed: true,
            succeeded: false,
            ..job("halyard/storefront", "build", "node", 0, 5)
        });
        assert_eq!(due(&config, &killed, 33 * G, &BTreeMap::new()), Vec::new());
    }

    #[test]
    fn two_profiles_never_both_take_a_slot_only_one_fits() {
        // Both 2 slots at 3G caps, their jobs peaking at 2G and waiting; a 1G mirror in an 11G slice. One more slot
        // fits (3 × 2G + 2 × 2G + 1G = 11G), two do not.
        let text = r#"
[host]
slice = "runnerd.slice"
isolation = "sysbox"
admission = "measured"

[caches]
mirror = { enabled = true, memory = "1G", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.jvm]
image_stage = "jvm"
cpus = 4
memory = "3G"
slots = 2
max_slots = 3

[profiles.node]
image_stage = "node"
cpus = 4
memory = "3G"
slots = 2
max_slots = 3

[[repositories]]
name = "halyard/ledger"
profile = "jvm"
labels = ["jvm"]

[[repositories]]
name = "halyard/storefront"
profile = "node"
labels = ["node"]
"#;
        let config = on(text, "");
        let waited = |repository: &str, profile: &str| -> Vec<Ran> {
            (0..10)
                .map(|_| Ran {
                    memory_cap: Some(3 * G),
                    ..job(repository, "build", profile, 2 * G, 300)
                })
                .collect()
        };
        let mut jobs = waited("halyard/ledger", "jvm");
        jobs.extend(waited("halyard/storefront", "node"));
        let raised = due(&config, &history(jobs), 11 * G, &BTreeMap::new());
        assert_eq!(
            raised.iter().map(|change| change.to.slots).sum::<u32>(),
            5,
            "one profile gets the third slot: {raised:?}"
        );
    }

    #[test]
    fn under_measured_admission_waiting_jobs_get_one_more_slot_up_to_max_slots_while_their_peaks_fit()
     {
        let text = measured().replace("slots = 4", "slots = 4\nmax_slots = 5");
        let config = on(&text, "");
        // Peaks of 4812M at a 5.5G cap: kept, and they waited 5 minutes.
        let waited = history(capped_builds(10, 4812 * M, 300));
        // jvm 2 × 9G (too few jobs: at its cap), node 5 × 4812M, the mirror 0.5G: 43004M.
        assert_eq!(due(&config, &waited, 41 * G, &BTreeMap::new()), Vec::new());
        let raised = due(&config, &waited, 42 * G, &BTreeMap::new());
        assert_eq!(
            resized(&raised),
            vec![(
                "node",
                Resize {
                    memory: 5632 * M,
                    slots: 5,
                    cpus: None
                }
            )]
        );
        let at_max = on(
            &measured().replace("slots = 4", "slots = 4\nmax_slots = 4"),
            "",
        );
        assert_eq!(due(&at_max, &waited, 64 * G, &BTreeMap::new()), Vec::new());
    }

    #[test]
    fn without_max_slots_waiting_jobs_get_a_slot_only_while_every_profiles_peaks_fit_the_slice() {
        let config = on(&measured(), "");
        let waited = history(capped_builds(10, 4812 * M, 300));
        // jvm 2 × 9G at its cap, node 5 × 4812M, the mirror 0.5G: 43004M.
        assert_eq!(
            due(&config, &waited, 41 * G, &BTreeMap::new()),
            Vec::new(),
            "a fifth slot does not fit 41G"
        );
        assert_eq!(
            resized(&due(&config, &waited, 42 * G, &BTreeMap::new())),
            vec![(
                "node",
                Resize {
                    memory: 5632 * M,
                    slots: 5,
                    cpus: None
                }
            )]
        );
        let unlimited = changes(
            &config,
            &waited,
            Context {
                memory_max: None,
                cpus: Some(32_000),
                window: window(),
                last: &BTreeMap::new(),
                running: &BTreeMap::new(),
            },
        );
        assert_eq!(
            unlimited,
            Vec::new(),
            "without max_slots or a MemoryMax nothing bounds slots"
        );
    }

    #[test]
    fn under_caps_waiting_jobs_get_a_slot_while_every_cap_fits_the_slice() {
        // node's builds peak at 6G: + 15% fits its 7G, so its memory stays, and they waited 5 minutes. 2 × 9G + 3 × 7G + 0.5G
        // = 39.5G.
        let config = on(CONFIG, "");
        let waited = history(builds(10, 5632 * M, 6 * G, 300));
        assert_eq!(due(&config, &waited, 39 * G, &BTreeMap::new()), Vec::new());
        assert_eq!(
            resized(&due(&config, &waited, 40 * G, &BTreeMap::new())),
            vec![(
                "node",
                Resize {
                    memory: 7 * G,
                    slots: 3,
                    cpus: None
                }
            )]
        );
    }

    #[test]
    fn a_young_profile_whose_jobs_wait_gets_a_slot_before_it_has_min_jobs_runs() {
        let config = on(&measured(), "");
        let waited = history(capped_builds(3, 4812 * M, 300));
        let raised = due(&config, &waited, 64 * G, &BTreeMap::new());
        assert_eq!(
            resized(&raised),
            vec![(
                "node",
                Resize {
                    memory: 5632 * M,
                    slots: 5,
                    cpus: None
                }
            )]
        );
        let two = history(capped_builds(2, 4812 * M, 300));
        assert_eq!(
            due(&config, &two, 64 * G, &BTreeMap::new()),
            Vec::new(),
            "two runs are too few even to add a slot"
        );
    }

    #[test]
    fn under_measured_admission_a_lowering_that_adds_a_slot_says_why_both() {
        let config = on(&measured(), "");
        // Peaks of 2G at a 5.5G cap, waiting 5 minutes: lower the cap and add a slot.
        let waited = history(capped_builds(10, 2 * G, 300));
        let changed = due(&config, &waited, 64 * G, &BTreeMap::new());
        let [change] = &changed[..] else {
            panic!("one change: {changed:?}")
        };
        assert!(
            change.to.memory < change.from.memory && change.to.slots == 5,
            "{change:?}"
        );
        assert!(
            change.reason.contains("lower memory") && change.reason.contains("waited 300s"),
            "the reason says both: {}",
            change.reason
        );
    }

    /// When node was last changed, `hours_ago`.
    fn node_changed(hours_ago: u64) -> BTreeMap<String, u64> {
        BTreeMap::from([("node".to_owned(), (NOW - hours_ago * HOUR) / 1000)])
    }

    #[test]
    fn a_raise_comes_after_the_raise_interval_while_a_lowering_waits_the_whole_interval() {
        let config = on(&measured(), "");
        // Queued half an hour ago, each waiting 5 minutes for a slot.
        let waited = history(
            capped_builds(10, 4812 * M, 300)
                .into_iter()
                .map(|build| Ran {
                    queued_at: NOW - HOUR / 2,
                    started_at: Some(NOW - HOUR / 2 + 300_000),
                    ..build
                })
                .collect(),
        );
        let changed_minutes_ago =
            |minutes: u64| BTreeMap::from([("node".to_owned(), (NOW - minutes * 60_000) / 1000)]);
        assert_eq!(
            due(&config, &waited, 64 * G, &changed_minutes_ago(59)),
            Vec::new(),
            "the hour's raise interval is not over"
        );
        let raised = due(&config, &waited, 64 * G, &changed_minutes_ago(61));
        assert_eq!(raised.first().map(|change| change.to.slots), Some(5));
        // Peaks far under node's cap: a lowering, which the 24 hours since the last change hold.
        let light = history(capped_builds(10, 2 * G, 5));
        assert_eq!(due(&config, &light, 64 * G, &node_changed(3)), Vec::new());
        assert_eq!(
            due(&config, &light, 64 * G, &node_changed(25))
                .first()
                .map(|change| change.to.memory < 5632 * M),
            Some(true),
            "lowered once the interval is over"
        );
    }

    #[test]
    fn a_raise_reads_only_the_waits_since_the_profiles_last_change() {
        let config = on(&measured(), "");
        // Jobs queued 2 hours ago waited; a slot was added an hour and a half ago, and nothing waited since.
        let mut jobs = capped_builds(10, 4812 * M, 300);
        jobs.extend((0..3).map(|_| Ran {
            queued_at: NOW - HOUR,
            started_at: Some(NOW - HOUR + 2000),
            ..capped_builds(1, 4812 * M, 0).remove(0)
        }));
        let changed = BTreeMap::from([("node".to_owned(), (NOW - 90 * 60_000) / 1000)]);
        assert_eq!(due(&config, &history(jobs), 64 * G, &changed), Vec::new());
    }

    /// `count` ledger tests on jvm's 8 CPUs, each peaking at `cpu_peak` millicores, at `cpu_cap`.
    fn ledger_cpu(count: usize, cpu_peak: u64, cpu_cap: u64) -> Vec<Ran> {
        (0..count)
            .map(|_| Ran {
                cpu_peak,
                cpu_cap: Some(cpu_cap),
                ..job("halyard/ledger", "test", "jvm", 7 * G, 5)
            })
            .collect()
    }

    /// The changes due at `NOW` in a 33G slice of `cpus` millicores, after the last changes `last`.
    fn due_cpus(
        config: &Config,
        history: &History,
        cpus: Option<u64>,
        last: &BTreeMap<String, u64>,
    ) -> Vec<SizingChange> {
        changes(
            config,
            history,
            Context {
                memory_max: Some(33 * G),
                cpus,
                window: window(),
                last,
                running: &BTreeMap::new(),
            },
        )
    }

    fn cpus_to(changes: &[SizingChange]) -> Vec<(&str, Option<u32>, Option<u32>)> {
        changes
            .iter()
            .map(|change| (change.profile.as_str(), change.from.cpus, change.to.cpus))
            .collect()
    }

    #[test]
    fn a_profile_whose_jobs_sit_at_their_cpu_cap_gets_more_cpus_up_to_the_slices() {
        let config = on(CONFIG, "");
        let pinned = history(ledger_cpu(3, 7900, 8000));
        let raised = due_cpus(&config, &pinned, Some(32_000), &BTreeMap::new());
        assert_eq!(
            cpus_to(&raised),
            vec![("jvm", Some(8), Some(10))],
            "8 CPUs + 25%"
        );
        assert_eq!(
            (raised[0].to.memory, raised[0].to.slots),
            (9 * G, 2),
            "memory and slots stay"
        );
        assert!(
            raised[0].line().contains("cpus 8 → 10"),
            "{}",
            raised[0].line()
        );
        assert_eq!(
            cpus_to(&due_cpus(&config, &pinned, Some(9_000), &BTreeMap::new())),
            vec![("jvm", Some(8), Some(9))],
            "at most the slice's CPUs"
        );
        assert_eq!(
            due_cpus(&config, &pinned, None, &BTreeMap::new()),
            Vec::new(),
            "not without the slice's CPUs"
        );
        let elsewhere = history(ledger_cpu(3, 5900, 6000));
        assert_eq!(
            due_cpus(&config, &elsewhere, Some(32_000), &BTreeMap::new()),
            Vec::new(),
            "runs at another cap say nothing about this one"
        );
        let two = history(ledger_cpu(2, 7900, 8000));
        assert_eq!(
            due_cpus(&config, &two, Some(32_000), &BTreeMap::new()),
            Vec::new()
        );
    }

    #[test]
    fn under_caps_a_cpu_raise_stays_within_the_cpu_budget() {
        // jvm 2 × 8 and node 2 × 8 CPUs: 32 of a 36-CPU budget leave room for jvm's slots at 10, not 12.
        let config = on(
            &CONFIG.replace(
                "isolation = \"sysbox\"",
                "isolation = \"sysbox\"\ncpu_overcommit = 1.5",
            ),
            "max_step = 50",
        );
        let pinned = history(ledger_cpu(3, 7900, 8000));
        assert_eq!(
            cpus_to(&due_cpus(&config, &pinned, Some(24_000), &BTreeMap::new())),
            vec![("jvm", Some(8), Some(10))]
        );
    }

    /// `CONFIG` (jvm and node 2 × 8 CPUs each) under a CPU budget of the slice's CPUs.
    fn cpu_budget(rules: &str) -> Config {
        on(
            &CONFIG.replace(
                "isolation = \"sysbox\"",
                "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
            ),
            rules,
        )
    }

    /// The changes due at `NOW` in a slice of `memory_max` and `cpus` millicores, with nothing changed before.
    fn due_in(config: &Config, history: &History, memory_max: u64, cpus: u64) -> Vec<SizingChange> {
        changes(
            config,
            history,
            Context {
                memory_max: Some(memory_max),
                cpus: Some(cpus),
                window: window(),
                last: &BTreeMap::new(),
                running: &BTreeMap::new(),
            },
        )
    }

    #[test]
    fn under_caps_a_waiting_profile_gets_no_slot_the_cpu_budget_cannot_hold() {
        // node's builds fit its 7G and waited 5 minutes; 2 × 8 + 2 × 8 CPUs fill a 32-CPU budget.
        let waited = history(builds(10, 5632 * M, 6 * G, 300));
        assert_eq!(due_in(&cpu_budget(""), &waited, 64 * G, 32_000), Vec::new());
        assert_eq!(
            resized(&due_in(&cpu_budget(""), &waited, 64 * G, 40_000)),
            vec![(
                "node",
                Resize {
                    memory: 7 * G,
                    slots: 3,
                    cpus: None
                }
            )],
            "40 CPUs hold one more slot of 8"
        );
    }

    #[test]
    fn under_caps_a_lowering_keeps_its_memory_but_adds_only_the_slots_the_cpu_budget_holds() {
        // The advice: 2.5G and 5 slots in the same 14G. A full 32-CPU budget keeps 2; 40 CPUs hold a third.
        let waited = history(builds(100, 1200 * M, 2150 * M, 300));
        let config = cpu_budget("max_step = 100");
        let full = due_in(&config, &waited, 64 * G, 32_000);
        assert_eq!(
            full.iter()
                .map(|change| (change.to.memory < change.from.memory, change.to.slots))
                .collect::<Vec<_>>(),
            [(true, 2)],
            "{full:?}"
        );
        let room = due_in(&config, &waited, 64 * G, 40_000);
        assert_eq!(
            room.iter()
                .map(|change| change.to.slots)
                .collect::<Vec<_>>(),
            [3]
        );
    }

    #[test]
    fn two_profiles_never_both_take_the_cpus_only_one_slot_fits() {
        let mut jobs: Vec<Ran> = (0..10)
            .map(|_| job("halyard/ledger", "test", "jvm", 7 * G, 300))
            .collect();
        jobs.extend(builds(10, 5632 * M, 6 * G, 300));
        let raised = due_in(&cpu_budget(""), &history(jobs), 64 * G, 40_000);
        assert_eq!(
            raised.iter().map(|change| change.to.slots).sum::<u32>(),
            3,
            "one profile gets the slot 8 more CPUs hold: {raised:?}"
        );
    }

    /// Three ledger tests pinned at jvm's 8 CPUs, queued `hours_ago`.
    fn pinned_hours_ago(hours_ago: u64) -> Vec<Ran> {
        ledger_cpu(3, 7900, 8000)
            .into_iter()
            .map(|run| Ran {
                queued_at: NOW - hours_ago * HOUR,
                started_at: Some(NOW - hours_ago * HOUR + 5000),
                ..run
            })
            .collect()
    }

    #[test]
    fn cpus_lowered_or_undone_back_to_a_cap_are_not_raised_again_by_its_old_runs() {
        let config = on(CONFIG, "");
        // Runs pinned at 8 CPUs raised jvm to 10 three days ago; ten light runs at 10 lowered it back to 8 two hours
        // ago (or an undo did). The pinned runs are from before that change: nothing new says 8 is too few.
        let mut jobs = pinned_hours_ago(80);
        jobs.extend(ledger_cpu(10, 2000, 10_000));
        let lowered = BTreeMap::from([("jvm".to_owned(), (NOW - 2 * HOUR) / 1000)]);
        assert_eq!(
            due_cpus(&config, &history(jobs.clone()), Some(32_000), &lowered),
            Vec::new()
        );
        jobs.extend(pinned_hours_ago(1));
        assert_eq!(
            cpus_to(&due_cpus(&config, &history(jobs), Some(32_000), &lowered)),
            vec![("jvm", Some(8), Some(10))],
            "runs pinned since the change raise it"
        );
    }

    #[test]
    fn cpus_far_above_every_peak_are_lowered_after_the_interval_by_at_most_a_step() {
        let config = on(CONFIG, "");
        // Ten runs peaking at 2 CPUs of 8: + 15% is 2.3, rounded up to 3, but a step is at most 25%: 6.
        let idle = history(ledger_cpu(10, 2000, 8000));
        assert_eq!(
            cpus_to(&due_cpus(&config, &idle, Some(32_000), &BTreeMap::new())),
            vec![("jvm", Some(8), Some(6))]
        );
        let jvm_changed = BTreeMap::from([("jvm".to_owned(), (NOW - 3 * HOUR) / 1000)]);
        assert_eq!(
            due_cpus(&config, &idle, Some(32_000), &jvm_changed),
            Vec::new(),
            "a lowering waits the interval"
        );
        assert_eq!(
            due_cpus(
                &config,
                &history(ledger_cpu(9, 2000, 8000)),
                Some(32_000),
                &BTreeMap::new()
            ),
            Vec::new(),
            "a lowering takes min_jobs runs"
        );
    }
}
