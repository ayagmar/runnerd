//! Auto sizing's `cpus`: raised for a job its cap holds back over the whole run, not for a build whose bursts reach
//! any cap under `caps` admission; raised under `measured` for runs the cap held back, throttled where runnerd
//! measured that, else bursting to it (ADRs 0075 and 0079); lowered under `caps` toward twice the heaviest job's
//! average or the runs' peak, never on runs the cap held back; and its invariants over generated sequences of runs,
//! idle and contended, looks, hand edits, undos and restarts.

use std::collections::BTreeMap;

use super::{CONFIG, G, HOUR, NOW, job};
use crate::api::{Resize, SizingChange};
use crate::config::{Admission, Config, cpu_budget, resize_profile};
use crate::sizing::auto::{Context, Look, next_look};
use crate::sizing::{CpusSet, History, Ran, Window, spread};

const DAY: u64 = 24 * HOUR;

/// `text` with `[sizing] auto` on and `rules` added.
fn on(text: &str, rules: &str) -> Config {
    // The pace these tests were written for, unless they set their own.
    let interval = if rules.lines().any(|line| line.starts_with("interval")) {
        ""
    } else {
        "interval = \"24h\"\n"
    };
    let step = if rules.lines().any(|line| line.starts_with("max_step")) {
        ""
    } else {
        "max_step = 25\n"
    };
    Config::parse(&format!(
        "{text}\n[sizing]\nauto = true\n{interval}{step}{rules}"
    ))
    .unwrap()
}

/// `CONFIG` with jvm at `cpus`.
fn jvm_at(cpus: u32, rules: &str) -> Config {
    on(
        &CONFIG.replacen("cpus = 8", &format!("cpus = {cpus}"), 1),
        rules,
    )
}

/// `jvm_at` under `admission = "measured"`.
fn measured_at(cpus: u32) -> Config {
    on(
        &CONFIG
            .replacen("cpus = 8", &format!("cpus = {cpus}"), 1)
            .replace(
                "isolation = \"sysbox\"",
                "isolation = \"sysbox\"\nadmission = \"measured\"",
            ),
        "",
    )
}

/// `CONFIG` (jvm and node 2 × 8 CPUs) under a CPU budget of the slice's CPUs.
fn budgeted(rules: &str) -> Config {
    on(
        &CONFIG.replace(
            "isolation = \"sysbox\"",
            "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
        ),
        rules,
    )
}

/// `count` runs of halyard/ledger's `name` in jvm at `cpu_cap` millicores, queued at `at`, each averaging `average`
/// and, as a parallel build does, peaking at the cap.
fn runs(name: &str, count: usize, average: u64, cpu_cap: u64, at: u64) -> Vec<Ran> {
    bursts(name, count, average, cpu_cap, cpu_cap, at)
}

/// `runs` whose bursts peak at `peak` millicores, under the cap when they need less.
fn bursts(name: &str, count: usize, average: u64, peak: u64, cpu_cap: u64, at: u64) -> Vec<Ran> {
    (0..count)
        .map(|_| Ran {
            cpu_peak: peak.min(cpu_cap),
            cpu_average: Some(average),
            cpu_cap: Some(cpu_cap),
            queued_at: at,
            started_at: Some(at + 5000),
            runner_started_at: at,
            finished_at: Some(at + HOUR / 2),
            ..job("halyard/ledger", name, "jvm", 7 * G, 5)
        })
        .collect()
}

fn history(jobs: Vec<Ran>) -> History {
    History {
        jobs,
        ..History::default()
    }
}

/// The look at `now` (ms) in a 33G slice of `cpus` millicores, after the last changes `last`, nothing running, the
/// daemon having run each profile's `cpus` as `cpus_set` says.
fn look_set(
    config: &Config,
    history: &History,
    cpus: Option<u64>,
    now: u64,
    last: &BTreeMap<String, u64>,
    cpus_set: &[CpusSet],
) -> Look {
    next_look(
        config,
        history,
        Context {
            memory_max: Some(33 * G),
            cpus,
            window: Window {
                since: now - 30 * DAY,
                now,
                sample_interval: 2,
            },
            last,
            cpus_set,
            running: &BTreeMap::new(),
            disk: None,
        },
    )
}

/// `look_set` with no `profile.changed` events.
fn look_at(
    config: &Config,
    history: &History,
    cpus: Option<u64>,
    now: u64,
    last: &BTreeMap<String, u64>,
) -> Look {
    look_set(config, history, cpus, now, last, &[])
}

fn look(config: &Config, history: &History, cpus: Option<u64>) -> Look {
    look_at(config, history, cpus, NOW, &BTreeMap::new())
}

/// Each change's profile and `cpus` from and to.
fn cpus_to(look: &Look) -> Vec<(&str, Option<u32>, Option<u32>)> {
    look.changes
        .iter()
        .map(|change| (change.profile.as_str(), change.from.cpus, change.to.cpus))
        .collect()
}

#[test]
fn a_parallel_build_that_peaks_at_any_cap_is_not_raised_while_its_runs_average_far_under_it() {
    // Like the Android quality gate on the owner's host: its bursts reach 8, 16 or 31 CPUs, whatever the cap, while
    // its runs average 6 of them. The old rule raised it on its peaks, 16 → 20 → 25 → 31 in a day.
    for cpus in [8, 16, 31] {
        let config = jvm_at(cpus, "");
        let built = history(runs(
            "android",
            3,
            5900.min(u64::from(cpus) * 650),
            u64::from(cpus) * 1000,
            NOW - HOUR,
        ));
        let look = look(&config, &built, Some(32_000));
        assert_eq!(look.changes, Vec::new(), "at {cpus} CPUs");
        assert_eq!(
            look.refused,
            Vec::new(),
            "nothing asks for a raise at {cpus}"
        );
    }
}

#[test]
fn a_job_whose_runs_average_at_the_cap_gets_more_cpus_up_to_the_slices() {
    let config = jvm_at(8, "");
    let pinned = history(runs("test", 3, 7900, 8000, NOW - HOUR));
    let raised = look(&config, &pinned, Some(32_000));
    assert_eq!(
        cpus_to(&raised),
        vec![("jvm", Some(8), Some(10))],
        "8 CPUs + 25%"
    );
    assert_eq!(
        (raised.changes[0].to.memory, raised.changes[0].to.slots),
        (9 * G, 2),
        "memory and slots stay"
    );
    assert!(
        raised.changes[0]
            .reason
            .contains("3 runs of halyard/ledger CI / test averaged 7.9 of its 8 CPUs (median)"),
        "{}",
        raised.changes[0].reason
    );
    assert_eq!(
        cpus_to(&look(&config, &pinned, Some(9_000))),
        vec![("jvm", Some(8), Some(9))],
        "at most the slice's CPUs"
    );
    let unknown = look(&config, &pinned, None);
    assert_eq!(unknown.changes, Vec::new(), "not without the slice's CPUs");
    assert!(
        unknown.refused[0]
            .reason
            .ends_with("the slice's CPUs could not be read"),
        "{:?}",
        unknown.refused
    );
    let full = look(&config, &pinned, Some(8_000));
    assert_eq!(full.changes, Vec::new());
    assert!(
        full.refused[0]
            .reason
            .ends_with("it has all 8 of the slice's CPUs"),
        "{:?}",
        full.refused
    );
    for (other, why) in [
        (
            runs("test", 3, 5900, 6000, NOW - HOUR),
            "runs at another cap",
        ),
        (runs("test", 2, 7900, 8000, NOW - HOUR), "two runs"),
    ] {
        assert_eq!(
            look(&config, &history(other), Some(32_000)).changes,
            Vec::new(),
            "{why} say nothing about this cap"
        );
    }
}

#[test]
fn one_job_held_back_by_the_cap_raises_it_however_light_the_profiles_other_jobs() {
    // Three image builds at 6 of 8 CPUs (75%) beside twenty lint runs at a tenth of a CPU: the profile's mean is far
    // under the cap, the build is not.
    let config = jvm_at(8, "");
    let mut jobs = runs("lint", 20, 100, 8000, NOW - HOUR);
    jobs.extend(runs("image", 3, 6000, 8000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look(&config, &history(jobs), Some(32_000))),
        vec![("jvm", Some(8), Some(10))]
    );
}

#[test]
fn failed_cancelled_and_unmeasured_runs_raise_nothing() {
    let config = jvm_at(8, "");
    let pinned = runs("test", 3, 7900, 8000, NOW - HOUR);
    let failed: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            succeeded: false,
            ..run.clone()
        })
        .collect();
    let unmeasured: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            cpu_average: None,
            ..run.clone()
        })
        .collect();
    let sampled_once: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            cpu_average: Some(0),
            ..run.clone()
        })
        .collect();
    let uncapped: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            cpu_cap: None,
            ..run.clone()
        })
        .collect();
    for (jobs, why) in [
        (failed, "failed"),
        (unmeasured, "without an average"),
        (sampled_once, "sampled once"),
        (uncapped, "without a recorded cap"),
    ] {
        assert_eq!(
            look(&config, &history(jobs), Some(32_000)).changes,
            Vec::new(),
            "runs {why} say nothing"
        );
    }
    let cancelled = History {
        jobs: Vec::new(),
        cancelled: pinned,
        runners: Vec::new(),
        ..History::default()
    };
    assert_eq!(
        look(&config, &cancelled, Some(32_000)).changes,
        Vec::new(),
        "cancelled runs say nothing"
    );
}

#[test]
fn an_over_provisioned_profile_steps_down_to_its_target_over_several_intervals_and_stops_there() {
    // jvm at 31 CPUs. Its heaviest job averages 5 to 7.75 CPUs, 7.75 at p95, and bursts to 20; twice the average is
    // 15.5, so the peak sets the target. Each day brings ten runs at the cap of that day, and a lint job averaging
    // half a CPU. At 20 the bursts meet the cap, and nothing comes down again.
    let mut config = jvm_at(31, "");
    let mut text = CONFIG.replacen("cpus = 8", "cpus = 31", 1);
    let mut jobs = Vec::new();
    let mut last = BTreeMap::new();
    let mut seen = vec![31];
    for day in 0..6 {
        let now = NOW + day * DAY;
        let cap = u64::from(config.profiles["jvm"].cpus) * 1000;
        for i in 0..10 {
            jobs.extend(bursts(
                "android",
                1,
                5000 + 250 * (i + 2),
                20_000,
                cap,
                now - HOUR,
            ));
        }
        jobs.extend(bursts("lint", 4, 500, 2000, cap, now - HOUR));
        let look = look_at(&config, &history(jobs.clone()), Some(32_000), now, &last);
        for change in &look.changes {
            text = resize_profile(&text, &change.profile, change.to).unwrap();
            last.insert(change.profile.clone(), change.at);
        }
        config = on(&text, "");
        seen.push(config.profiles["jvm"].cpus);
        if let Some(change) = look.changes.first() {
            assert!(
                change
                    .reason
                    .contains("peaked at 20 CPUs at most, none held back by the cap, and the heaviest job, halyard/ledger \
                               CI / android, averaged 7.8 CPUs (p95); the larger of that peak and twice that \
                               average is 20"),
                "{}",
                change.reason
            );
        }
    }
    assert_eq!(
        seen,
        [31, 24, 20, 20, 20, 20, 20],
        "a step of at most 25% a day, down to the peak"
    );
}

#[test]
fn a_lowering_waits_the_interval_and_min_jobs_runs_since_the_last_change() {
    let config = jvm_at(8, "");
    let idle = history(bursts("test", 10, 1000, 2000, 8000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look(&config, &idle, Some(32_000))),
        vec![("jvm", Some(8), Some(6))],
        "twice 1 CPU is 2, but a step is at most 25%"
    );
    let changed = BTreeMap::from([("jvm".to_owned(), (NOW - 3 * HOUR) / 1000)]);
    let held = look_at(&config, &idle, Some(32_000), NOW, &changed);
    assert_eq!(held.changes, Vec::new(), "a lowering waits the interval");
    let before = BTreeMap::from([("jvm".to_owned(), (NOW - 25 * HOUR) / 1000)]);
    let mut old = bursts("test", 9, 1000, 2000, 8000, NOW - 26 * HOUR);
    old.extend(bursts("test", 9, 1000, 2000, 8000, NOW - HOUR));
    assert_eq!(
        look_at(&config, &history(old), Some(32_000), NOW, &before).changes,
        Vec::new(),
        "9 runs since the last change are fewer than min_jobs; older ones do not count"
    );
}

#[test]
fn a_cap_lowered_back_by_auto_sizing_reads_only_the_runs_since() {
    let config = jvm_at(8, "");
    // Runs pinned at 8 CPUs, a raise to 10, ten light runs there, and a lowering back to 8 two hours ago.
    let mut jobs = runs("test", 3, 7900, 8000, NOW - 80 * HOUR);
    jobs.extend(runs("test", 10, 2000, 10_000, NOW - 3 * HOUR));
    let lowered = BTreeMap::from([("jvm".to_owned(), (NOW - 2 * HOUR) / 1000)]);
    assert_eq!(
        look_at(&config, &history(jobs.clone()), Some(32_000), NOW, &lowered).changes,
        Vec::new(),
        "the old runs at 8 say nothing new"
    );
    jobs.extend(runs("test", 3, 7900, 8000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look_at(
            &config,
            &history(jobs),
            Some(32_000),
            NOW,
            &lowered
        )),
        vec![("jvm", Some(8), Some(10))],
        "runs at the cap since then raise it"
    );
}

/// jvm's `cpus` as the daemon ran them, each from `hours_ago`.
fn ran_at(set: &[(u32, u64)]) -> Vec<CpusSet> {
    set.iter()
        .map(|&(cpus, hours_ago)| CpusSet {
            profile: "jvm".to_owned(),
            cpus,
            at: NOW - hours_ago * HOUR,
        })
        .collect()
}

#[test]
fn a_cap_edited_or_undone_away_and_back_reads_only_the_runs_since_even_with_none_between() {
    let config = jvm_at(8, "");
    // Tests pinned at 8 CPUs and ten idle lint runs, then a reload or an undo to 10 and one back to 8, no run at 10:
    // nothing auto sizing recorded, and no run under another cap, marks the return.
    let mut jobs = runs("test", 3, 7900, 8000, NOW - 6 * HOUR);
    jobs.extend(runs("lint", 10, 500, 8000, NOW - 6 * HOUR));
    let pinned = history(jobs.clone());
    let none = BTreeMap::new();
    let back = ran_at(&[(8, 40 * 24), (10, 5), (8, 4)]);
    let looked = look_set(&config, &pinned, Some(32_000), NOW, &none, &back);
    assert_eq!(
        (looked.changes, looked.refused),
        (Vec::new(), Vec::new()),
        "neither a raise nor a lowering on the runs before"
    );
    // A restart announces the profile again at the same CPUs: the return still counts from when it came.
    let restarted = ran_at(&[(8, 40 * 24), (10, 5), (8, 4), (8, 1)]);
    assert_eq!(
        look_set(&config, &pinned, Some(32_000), NOW, &none, &restarted).changes,
        Vec::new()
    );
    // Edited back to 10 while the daemon was down, and not announced yet: runs at 10 from before say nothing.
    let mut old_ten = jobs.clone();
    old_ten.extend(runs("test", 3, 9500, 10_000, NOW - 4 * HOUR - HOUR / 2));
    assert_eq!(
        look_set(
            &jvm_at(10, ""),
            &history(old_ten),
            Some(32_000),
            NOW,
            &none,
            &back
        )
        .changes,
        Vec::new(),
        "the daemon has not run with them yet"
    );
    jobs.extend(runs("test", 3, 7900, 8000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look_set(
            &config,
            &history(jobs),
            Some(32_000),
            NOW,
            &none,
            &restarted
        )),
        vec![("jvm", Some(8), Some(10))],
        "runs at the cap since the return raise it"
    );
}

#[test]
fn a_job_without_a_measured_average_keeps_the_cpus_from_coming_down() {
    // Ten lint runs a tenth of a CPU, and three builds at the same cap whose averages were never measured: the lint
    // runs alone would bring 8 CPUs down toward 1, with nothing known of what the builds need.
    let config = jvm_at(8, "");
    let builds = bursts("build", 3, 3000, 5000, 8000, NOW - HOUR);
    let mut jobs = bursts("lint", 10, 100, 1000, 8000, NOW - HOUR);
    jobs.extend(builds.iter().cloned().map(|run| Ran {
        cpu_average: None,
        ..run
    }));
    assert_eq!(
        look(&config, &history(jobs.clone()), Some(32_000)).changes,
        Vec::new(),
        "no lowering past a job it knows nothing of"
    );
    let mut sampled_once = bursts("lint", 10, 100, 1000, 8000, NOW - HOUR);
    sampled_once.extend(builds.iter().cloned().map(|run| Ran {
        cpu_average: Some(0),
        ..run
    }));
    assert_eq!(
        look(&config, &history(sampled_once), Some(32_000)).changes,
        Vec::new(),
        "nor of one sampled once"
    );
    jobs.extend(builds);
    assert_eq!(
        cpus_to(&look(&config, &history(jobs), Some(32_000))),
        vec![("jvm", Some(8), Some(6))],
        "measured, the builds' 3 CPUs ask for 6: a step to 6"
    );
}

#[test]
fn a_day_of_contention_lowers_a_busy_job_a_step_and_it_asks_the_step_back_once_it_is_over() {
    // A job averaging 14 of its 16 CPUs, held to 2 by other programs on the host for a whole day, its bursts to 3:
    // the lowering cannot tell, and takes one step toward 4. Once the host is free the job sits at its new 12 and is
    // raised again. The step and the interval bound what contention costs; nothing in the runs tells it apart.
    let config = jvm_at(16, "");
    let slowed = history(bursts("build", 10, 2000, 3000, 16_000, NOW - HOUR));
    let day_ago = BTreeMap::from([("jvm".to_owned(), (NOW - 25 * HOUR) / 1000)]);
    let lowered = look_at(&config, &slowed, Some(32_000), NOW, &day_ago);
    assert_eq!(cpus_to(&lowered), vec![("jvm", Some(16), Some(12))]);
    let config = jvm_at(12, "");
    let free = history(runs("build", 3, 11_500, 12_000, NOW + HOUR));
    let changed = BTreeMap::from([("jvm".to_owned(), NOW / 1000)]);
    assert_eq!(
        cpus_to(&look_at(
            &config,
            &free,
            Some(32_000),
            NOW + 2 * HOUR,
            &changed
        )),
        vec![("jvm", Some(12), Some(15))]
    );
}

/// Ten runs at `cap` CPUs of a build like the owner's Android quality gate, queued at `at`: 371 s of steps near
/// one CPU and 1,313 CPU-seconds that spread over up to 23.5 CPUs. Its bursts meet any cap under 23.5, and its
/// average drops with the cap, as the time it spends held, and its run, grow.
fn gate(cap: u32, at: u64) -> Vec<Ran> {
    let cap = u64::from(cap) * 1000;
    (0..10)
        .flat_map(|i| {
            let wide = 23_500 - 100 * i;
            let used = wide.min(cap);
            let took = 371_000 + 1_313_000 * 1000 / used;
            let average = (371_000 + 1_313_000) * 1000 / took;
            bursts("android", 1, average, used, cap, at)
                .into_iter()
                .map(move |run| Ran {
                    finished_at: run.started_at.map(|started| started + took),
                    ..run
                })
        })
        .collect()
}

#[test]
fn cpus_the_bursts_meet_are_never_lowered_on_the_averages_they_hold_down() {
    // Falling caps, as auto sizing's old lowering or hand edits set them: 31 → 16 → 10 → 9 on the owner's host. At
    // each, the gate's average drops with the cap; twice it asked for a lower cap every day. A cap its bursts meet
    // is never lowered; one they do not is lowered no further than their peak, which they then never meet.
    for cpus in [31, 28, 24, 20, 16, 12, 10, 9, 7, 4] {
        let day_ago = BTreeMap::from([("jvm".to_owned(), (NOW - 25 * HOUR) / 1000)]);
        let ran = history(gate(cpus, NOW - HOUR));
        let caps = look_at(&jvm_at(cpus, ""), &ran, Some(32_000), NOW, &day_ago);
        for change in &caps.changes {
            let to = change.to.cpus.unwrap_or(cpus);
            assert!(
                to >= 24 && cpus >= 25,
                "at {cpus} CPUs, lowered to {to}: {}",
                change.reason
            );
        }
        if cpus <= 24 {
            assert_eq!(
                caps.changes,
                Vec::new(),
                "at {cpus} CPUs the bursts meet the cap"
            );
        }
        let measured = look_at(&measured_at(cpus), &ran, Some(32_000), NOW, &day_ago);
        let raised: Vec<u32> = measured
            .changes
            .iter()
            .filter_map(|change| change.to.cpus)
            .collect();
        if cpus <= 24 {
            assert!(
                raised.len() == 1 && raised[0] > cpus,
                "under measured admission the bursts at {cpus} raise it: {raised:?}"
            );
        } else {
            assert_eq!(raised, Vec::<u32>::new(), "under measured {cpus} CPUs stay");
        }
    }
    // The gate's own lowering, look after look: down to its peak, and no further.
    let mut text = CONFIG.replacen("cpus = 8", "cpus = 31", 1);
    let mut jobs = Vec::new();
    let mut last = BTreeMap::new();
    for day in 0..10 {
        let now = NOW + day * DAY;
        let config = on(&text, "");
        jobs.extend(gate(config.profiles["jvm"].cpus, now - HOUR));
        let look = look_at(&config, &history(jobs.clone()), Some(32_000), now, &last);
        for change in &look.changes {
            text = resize_profile(&text, &change.profile, change.to).unwrap();
            last.insert(change.profile.clone(), change.at);
        }
    }
    assert_eq!(
        on(&text, "").profiles["jvm"].cpus,
        24,
        "31 → 24, the gate's peak"
    );
}

#[test]
fn under_measured_admission_a_cap_the_bursts_meet_goes_back_up_until_they_fit() {
    // The owner's host: measured admission, jvm cut to 9 CPUs by the old lowering, the Android gate peaking at the
    // cap and averaging 3.3 of it, far under the 70% a raise on averages takes. Under measured admission the cap
    // bounds no slot, so runs at it raise it a step a look, to the first cap over the gate's 23.5-CPU bursts. Under
    // caps nothing moves: neither the bursts nor the averages raise it there, and they hold the lowering.
    let caps = CONFIG.replacen("cpus = 8", "cpus = 9", 1);
    let measured = caps.replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\nadmission = \"measured\"",
    );
    for (start, end) in [(measured, 28), (caps, 9)] {
        let mut text = start;
        let mut jobs = Vec::new();
        let mut last = BTreeMap::new();
        let mut seen = vec![9];
        for look in 0..8 {
            let now = NOW + look * 2 * HOUR;
            let config = on(&text, "max_step = 50");
            jobs.extend(gate(config.profiles["jvm"].cpus, now - HOUR));
            let looked = look_at(&config, &history(jobs.clone()), Some(32_000), now, &last);
            for change in &looked.changes {
                text = resize_profile(&text, &change.profile, change.to).unwrap();
                last.insert(change.profile.clone(), change.at);
            }
            seen.push(on(&text, "max_step = 50").profiles["jvm"].cpus);
        }
        assert_eq!(*seen.last().unwrap(), end, "{seen:?}");
        assert!(seen.is_sorted(), "never down: {seen:?}");
    }
}

/// `gate` with what its quota throttled, as runnerd measures from schema 17 on: busy all through, in 100 ms periods,
/// and throttled in each period of its wide phase that wants more than the cap. At 9 CPUs that phase takes 146 s of
/// its 517, 28% of its periods.
fn throttled_gate(cap: u32, at: u64) -> Vec<Ran> {
    let millicores = u64::from(cap) * 1000;
    gate(cap, at)
        .into_iter()
        .enumerate()
        .map(|(i, mut run)| {
            let wide = 23_500 - 100 * i as u64;
            let used = wide.min(millicores);
            let phase = 1_313_000 * 1000 / used;
            let took = 371_000 + phase;
            run.cpu_periods = Some(took / 100);
            run.cpu_throttled = Some(if wide > millicores { phase / 100 } else { 0 });
            run
        })
        .collect()
}

/// Where `cpus` of jvm go over 8 looks 2 hours apart, starting at `start` CPUs under `admission`, each look's runs
/// made by `runs` at the jvm's `cpus` then.
fn climb(admission: &str, start: u32, runs: fn(u32, u64) -> Vec<Ran>) -> Vec<u32> {
    let mut text = CONFIG
        .replacen("cpus = 8", &format!("cpus = {start}"), 1)
        .replace(
            "isolation = \"sysbox\"",
            &format!("isolation = \"sysbox\"\nadmission = \"{admission}\""),
        );
    let mut jobs = Vec::new();
    let mut last = BTreeMap::new();
    let mut seen = vec![start];
    for look in 0..8 {
        let now = NOW + look * 2 * HOUR;
        let config = on(&text, "max_step = 50");
        jobs.extend(runs(config.profiles["jvm"].cpus, now - HOUR));
        let looked = look_at(&config, &history(jobs.clone()), Some(32_000), now, &last);
        for change in &looked.changes {
            text = resize_profile(&text, &change.profile, change.to).unwrap();
            last.insert(change.profile.clone(), change.at);
        }
        seen.push(on(&text, "max_step = 50").profiles["jvm"].cpus);
    }
    seen
}

#[test]
fn under_measured_admission_a_cap_that_throttles_the_runs_goes_up_until_it_does_not() {
    // The gate at 9 CPUs, throttled in 28% of its run: a step a look to the first cap its quota no longer
    // throttles it at, as the bursts did before runnerd measured that.
    let seen = climb("measured", 9, throttled_gate);
    assert_eq!(seen, [9, 13, 19, 28, 28, 28, 28, 28, 28], "{seen:?}");
    let raised = look(
        &measured_at(9),
        &history(throttled_gate(9, NOW - HOUR)),
        Some(32_000),
    );
    assert!(
        raised.changes[0]
            .reason
            .contains("10 runs of halyard/ledger CI / android were held back by its 9 CPUs, throttled in 15% of their run or more:"),
        "{}",
        raised.changes[0].reason
    );
    // Under caps neither the throttling nor the averages raise it there, and the throttling holds the lowering.
    assert_eq!(climb("caps", 9, throttled_gate), [9; 9]);
}

#[test]
fn a_job_idle_between_a_few_bursts_is_held_back_by_its_run_not_by_its_busy_time() {
    // An 8-minute integration test: it waits on its services and the network for most of its run, and three 20-second
    // compiles want 20 CPUs. The kernel counts the 600 periods it had work in and throttles 550 of them, 92% of its
    // busy time, but 11% of its run; the cap cost it about 30 s of 480.
    let idle: Vec<Ran> = runs("integration", 6, 1_500, 9_000, NOW - HOUR)
        .into_iter()
        .map(|run| Ran {
            finished_at: run.started_at.map(|started| started + 8 * 60_000),
            cpu_periods: Some(600),
            cpu_throttled: Some(550),
            ..run
        })
        .collect();
    let looked = look(&measured_at(9), &history(idle.clone()), Some(32_000));
    assert_eq!(cpus_to(&looked), [], "a few bursts raise nothing");
    // The same throttling over a 1-minute run is most of it.
    let busy: Vec<Ran> = idle
        .into_iter()
        .map(|run| Ran {
            finished_at: run.started_at.map(|started| started + 60_000),
            ..run
        })
        .collect();
    let looked = look(&measured_at(9), &history(busy), Some(32_000));
    assert_eq!(cpus_to(&looked), [("jvm", Some(9), Some(11))]);
}

#[test]
fn a_raise_that_did_not_help_a_job_whose_pool_follows_the_quota_is_the_last() {
    // A Gradle build whose workers follow the quota (`availableProcessors`): at any cap it starts more workers than
    // it may run, so its quota throttles it in the same share of its run, and it takes as long. The first raise is
    // made; the next look sees it did not help, and holds, saying why.
    let pool = |cap: u32, at: u64| -> Vec<Ran> {
        let millicores = u64::from(cap) * 1000;
        bursts("gradle", 4, millicores / 2, millicores, millicores, at)
            .into_iter()
            .map(|run| Ran {
                finished_at: run.started_at.map(|started| started + 10 * 60_000),
                cpu_periods: Some(6_000),
                cpu_throttled: Some(2_400),
                ..run
            })
            .collect()
    };
    let seen = climb("measured", 9, pool);
    assert_eq!(seen, [9, 13, 13, 13, 13, 13, 13, 13, 13], "{seen:?}");
    let mut jobs = pool(9, NOW - 3 * HOUR);
    jobs.extend(pool(13, NOW - HOUR));
    let held = look(&measured_at(13), &history(jobs), Some(32_000));
    assert_eq!(cpus_to(&held), []);
    assert!(
        held.refused[0].reason.ends_with(
            "the raise from 9 to its 13 CPUs did not help it: its runs took 10m 00s at the median, against 10m 00s \
             before, its quota throttled them in 40% of their run, against 40%"
        ),
        "{}",
        held.refused[0].reason
    );
}

#[test]
fn a_raise_that_helped_one_held_job_goes_ahead_though_it_did_not_help_a_heavier_one() {
    // At 13 CPUs, raised from 9: the Gradle build stays throttled and takes as long, but the Android gate got faster.
    // The build has more held runs, yet the gate's says the raise helped, so the next one goes ahead.
    let pool = |cap: u64, at: u64| -> Vec<Ran> {
        bursts("gradle", 6, cap / 2, cap, cap, at)
            .into_iter()
            .map(|run| Ran {
                finished_at: run.started_at.map(|started| started + 10 * 60_000),
                cpu_periods: Some(6_000),
                cpu_throttled: Some(2_400),
                ..run
            })
            .collect()
    };
    let mut jobs = pool(9_000, NOW - 3 * HOUR);
    jobs.extend(throttled_gate(9, NOW - 3 * HOUR));
    jobs.extend(pool(13_000, NOW - HOUR));
    let gate = throttled_gate(13, NOW - HOUR);
    jobs.extend(gate.iter().take(4).cloned());
    let looked = look(&measured_at(13), &history(jobs.clone()), Some(32_000));
    assert_eq!(cpus_to(&looked), [("jvm", Some(13), Some(16))]);
    assert!(
        looked.changes[0]
            .reason
            .contains("halyard/ledger CI / android"),
        "{}",
        looked.changes[0].reason
    );
    // Without the gate's runs at 13, only the build is held, and its raise did not help: refused.
    jobs.retain(|run| !(run.name == "android" && run.cpu_cap == Some(13_000)));
    let held = look(&measured_at(13), &history(jobs), Some(32_000));
    assert_eq!(cpus_to(&held), []);
    assert!(
        held.refused[0]
            .reason
            .contains("halyard/ledger CI / gradle"),
        "{}",
        held.refused[0].reason
    );
}

#[test]
fn a_peak_at_the_cap_raises_nothing_once_the_quota_says_it_did_not_throttle_the_run() {
    // A contended host: the host's other programs hold the CPUs, so the runs get less than they ask without their
    // quota throttling them, and a 2-second interval at the cap now and then is no sign of the cap's cost. The
    // slice, not the profile, is short: more `cpus` would not help.
    let contended: Vec<Ran> = runs("android", 6, 3_300, 9_000, NOW - HOUR)
        .into_iter()
        .map(|run| Ran {
            cpu_periods: Some(5_000),
            cpu_throttled: Some(20),
            ..run
        })
        .collect();
    let looked = look(&measured_at(9), &history(contended.clone()), Some(32_000));
    assert_eq!(cpus_to(&looked), [], "no raise without throttling");
    assert_eq!(looked.refused, []);
    // The same runs before runnerd measured throttling raise it, as their peaks said before.
    let unmeasured: Vec<Ran> = contended
        .into_iter()
        .map(|run| Ran {
            cpu_periods: None,
            cpu_throttled: None,
            ..run
        })
        .collect();
    let looked = look(&measured_at(9), &history(unmeasured), Some(32_000));
    assert_eq!(cpus_to(&looked), [("jvm", Some(9), Some(11))]);
}

#[test]
fn after_the_upgrade_runs_with_and_without_a_throttling_figure_each_say_what_they_can() {
    let config = measured_at(9);
    let unmeasured = runs("android", 2, 3_300, 9_000, NOW - HOUR);
    let throttled = |throttled| {
        runs("android", 1, 3_300, 9_000, NOW - HOUR)
            .into_iter()
            .map(move |run| Ran {
                cpu_periods: Some(5_000),
                cpu_throttled: Some(throttled),
                ..run
            })
    };
    // Two runs from before that met the cap and one since that the quota throttled in a sixth of its 30 minutes:
    // three held back.
    let mut jobs = unmeasured.clone();
    jobs.extend(throttled(3_000));
    let looked = look(&config, &history(jobs), Some(32_000));
    assert_eq!(cpus_to(&looked), [("jvm", Some(9), Some(11))]);
    assert!(
        looked.changes[0]
            .reason
            .contains("or peaking at the cap where that was not measured"),
        "{}",
        looked.changes[0].reason
    );
    // Two from before and three since that it never throttled: two held back, too few.
    let mut jobs = unmeasured;
    jobs.extend((0..3).flat_map(|_| throttled(0)));
    assert_eq!(cpus_to(&look(&config, &history(jobs), Some(32_000))), []);
}

#[test]
fn under_caps_admission_a_cap_that_throttled_a_run_is_not_lowered_whatever_its_peak() {
    // 31 CPUs, the gate's bursts peaking at 20 as 2-second samples see them, but within those 2 seconds its quota
    // throttled it in a sixth of its 30-minute run: its averages say what the cap allowed, not what it needs.
    let day_ago = BTreeMap::from([("jvm".to_owned(), (NOW - 25 * HOUR) / 1000)]);
    let at = |throttled| {
        bursts("android", 10, 3_900, 20_000, 31_000, NOW - HOUR)
            .into_iter()
            .map(|run| Ran {
                cpu_periods: Some(8_000),
                cpu_throttled: Some(throttled),
                ..run
            })
            .collect::<Vec<_>>()
    };
    let held = look_at(
        &jvm_at(31, ""),
        &history(at(3_000)),
        Some(32_000),
        NOW,
        &day_ago,
    );
    assert_eq!(cpus_to(&held), []);
    let free = look_at(
        &jvm_at(31, ""),
        &history(at(0)),
        Some(32_000),
        NOW,
        &day_ago,
    );
    assert_eq!(cpus_to(&free), [("jvm", Some(31), Some(24))]);
}

/// The owner's host: measured admission with a CPU budget of 1.5 times a 32-CPU slice, jvm cut to 9 CPUs and node to
/// 7, a warm runner in each of jvm, node, light and caches.
const LIVE: &str = r#"
[host]
slice = "runnerd.slice"
isolation = "sysbox"
admission = "measured"
cpu_overcommit = 1.5

[caches]
mirror = { enabled = true, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.caches]
image_stage = "node"
cpus = 2
memory = "2G"
slots = 2

[profiles.jvm]
image_stage = "jvm"
cpus = 9
memory = "13.25G"
slots = 4

[profiles.light]
image_stage = "node"
cpus = 2
memory = "2G"
slots = 4

[profiles.node]
image_stage = "node"
cpus = 7
memory = "9.75G"
slots = 10

[[repositories]]
name = "halyard/ledger"
profile = "jvm"
labels = ["jvm"]

[[repositories]]
name = "halyard/storefront"
profile = "node"
labels = ["node"]

[[repositories]]
name = "halyard/lint"
profile = "light"
labels = ["light"]

[[repositories]]
name = "halyard/mirror"
profile = "caches"
labels = ["caches"]
"#;

#[test]
fn under_measured_admission_with_a_cpu_budget_a_raise_stops_where_warm_runners_would_fill_it() {
    // The gate bursts to 23.5 CPUs and node's builds to 16.5, so their runs meet any cap under that. Without a CPU
    // budget both would climb past 24 and 17, and their idle warm runners alone would reserve more than its 48 CPUs.
    // With it, each raise stops where every warm runner at its cap and one more runner at the largest cap fit.
    let budget = cpu_budget(32_000, 1.5);
    let mut text = LIVE.to_owned();
    let mut jobs = Vec::new();
    let mut last = BTreeMap::new();
    let mut seen = Vec::new();
    for look in 0..8 {
        let now = NOW + look * 2 * HOUR;
        let config = on(&text, "max_step = 50");
        let (jvm, node) = (config.profiles["jvm"].cpus, config.profiles["node"].cpus);
        seen.push((jvm, node));
        let warm: u32 = config.profiles.values().map(|profile| profile.cpus).sum();
        let largest = config
            .profiles
            .values()
            .map(|profile| profile.cpus)
            .max()
            .unwrap();
        assert!(
            u64::from(warm + largest) * 1000 <= budget,
            "warm runners and one more at {jvm}/{node} CPUs fit {budget}"
        );
        jobs.extend(gate(jvm, now - HOUR));
        let node_cap = u64::from(node) * 1000;
        jobs.extend(
            bursts("build", 10, 4000, 16_500, node_cap, now - HOUR)
                .into_iter()
                .map(|run| Ran {
                    repository: "halyard/storefront".to_owned(),
                    profile: "node".to_owned(),
                    memory_cap: Some(config.profiles["node"].memory.0),
                    ..run
                }),
        );
        let looked = look_at(&config, &history(jobs.clone()), Some(32_000), now, &last);
        for change in &looked.changes {
            text = resize_profile(&text, &change.profile, change.to).unwrap();
            last.insert(change.profile.clone(), change.at);
        }
    }
    assert_eq!(
        seen.last(),
        Some(&(17, 10)),
        "raised, then held at the budget: {seen:?}"
    );
    assert!(
        seen.windows(2)
            .all(|pair| pair[0].0 <= pair[1].0 && pair[0].1 <= pair[1].1),
        "{seen:?}"
    );
}

#[test]
fn under_measured_admission_cpus_never_come_down() {
    // Measured admission reserves a job's predicted average, not its cap: a lower cap frees no slot and only slows
    // the bursts. Ten light runs far under 16 CPUs lower them under caps, never under measured.
    let light = history(bursts("lint", 10, 500, 2000, 16_000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look(&jvm_at(16, ""), &light, Some(32_000))),
        vec![("jvm", Some(16), Some(12))],
        "under caps"
    );
    assert_eq!(
        look(&measured_at(16), &light, Some(32_000)).changes,
        Vec::new(),
        "under measured"
    );
}

#[test]
fn a_raise_that_does_not_fit_the_cpu_budget_is_not_written_and_says_why() {
    // jvm 2 × 8 and node 2 × 8 fill a 32-CPU budget: say node's memory lowering added slots once a lowering of its
    // cpus freed the budget. jvm's tests now sit at its cap: no CPU is left for them.
    let config = budgeted("");
    let pinned = history(runs("test", 3, 7900, 8000, NOW - HOUR));
    let full = look(&config, &pinned, Some(32_000));
    assert_eq!(full.changes, Vec::new());
    assert_eq!(full.refused.len(), 1);
    assert_eq!(full.refused[0].profile, "jvm");
    assert!(
        full.refused[0].reason.ends_with(
            "9 CPUs for each of its 2 slots do not fit the CPU budget beside the other profiles'"
        ),
        "{}",
        full.refused[0].reason
    );
    assert_eq!(
        cpus_to(&look(&config, &pinned, Some(36_000))),
        vec![("jvm", Some(8), Some(10))],
        "4 CPUs more hold 2 slots at 10"
    );
}

#[test]
fn a_slot_the_cpu_budget_refuses_does_not_also_refuse_a_cpu_raise_that_fits_without_it() {
    // jvm's three runs sat at its 8 CPUs and waited 5 minutes. A 36-CPU budget beside node's 16 holds 2 jvm slots at
    // 10 CPUs, not 3 at 8.
    let waited: Vec<Ran> = runs("test", 3, 7900, 8000, NOW - HOUR)
        .into_iter()
        .map(|run| Ran {
            started_at: Some(run.queued_at + 300_000),
            ..run
        })
        .collect();
    let raised = next_look(
        &budgeted(""),
        &history(waited),
        Context {
            memory_max: Some(64 * G),
            cpus: Some(36_000),
            window: Window {
                since: NOW - 30 * DAY,
                now: NOW,
                sample_interval: 2,
            },
            last: &BTreeMap::new(),
            cpus_set: &[],
            running: &BTreeMap::new(),
            disk: None,
        },
    );
    assert_eq!(
        raised
            .changes
            .iter()
            .map(|change| (change.profile.as_str(), change.to.slots, change.to.cpus))
            .collect::<Vec<_>>(),
        [("jvm", 2, Some(10))],
        "{raised:?}"
    );
}

#[test]
fn memory_comes_down_once_the_kill_and_the_large_peaks_leave_the_history() {
    // A run killed for memory at jvm's 9G cap 29 days ago, and ten runs since peaking at 3G. Sizing reads 30 days:
    // the kill holds the cap until it is older than that, then the lowering comes, a step at a time.
    let config = jvm_at(8, "");
    let killed = Ran {
        oom_killed: true,
        ..runs("test", 1, 4000, 8000, NOW - 29 * DAY).remove(0)
    };
    let light: Vec<Ran> = runs("test", 10, 4000, 8000, NOW - HOUR)
        .into_iter()
        .map(|run| Ran {
            memory_peak: 3 * G,
            ..run
        })
        .collect();
    let read = |now: u64| {
        let mut jobs = vec![killed.clone()];
        jobs.extend(light.iter().cloned());
        jobs.retain(|job| job.queued_at >= now - 30 * DAY);
        look_at(&config, &history(jobs), Some(32_000), now, &BTreeMap::new())
    };
    assert_eq!(read(NOW).changes, Vec::new(), "the kill holds the cap");
    let later = read(NOW + 2 * DAY);
    assert_eq!(
        later
            .changes
            .iter()
            .map(|change| (change.profile.as_str(), change.to.memory))
            .collect::<Vec<_>>(),
        [("jvm", 6912 * (1 << 20))],
        "9G less 25%, rounded up to 256M: {later:?}"
    );
}

/// The invariants of `cpus` over generated sequences. The sequences come from a seeded generator, so a failure
/// replays: it prints its seed and the events up to it. `RUNNERD_INVARIANT_CASES` runs more cases than the default,
/// `RUNNERD_INVARIANT_SEED` one seed.
mod invariants {
    use std::fmt::Write as _;

    use super::*;

    const CASES: u64 = 64;
    const EVENTS: usize = 80;
    const PROFILES: [&str; 2] = ["jvm", "node"];
    /// The jobs a profile runs, each with how heavy it tends to be, in percent of the CPUs it gets, and how far its
    /// bursts reach, in CPUs: under a lower cap they meet it, and its average drops with it.
    const JOBS: [(&str, u64, u64); 4] = [
        ("build", 35, 24),
        ("lint", 5, 2),
        ("image", 75, 12),
        ("e2e", 95, 40),
    ];

    /// A deterministic generator (`SplitMix64`), as the engine's invariants use.
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

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[usize::try_from(self.below(items.len() as u64)).unwrap()]
        }
    }

    /// What the store keeps, as a restarted daemon reads it back: the runs, auto sizing's change records and each
    /// `profile.changed` event.
    struct Stored {
        jobs: Vec<Ran>,
        cancelled: Vec<Ran>,
        /// Auto sizing's changes: the profile, and when in seconds.
        changes: Vec<(String, u64)>,
        events: Vec<CpusSet>,
    }

    /// What a running daemon holds between looks, kept as it goes.
    struct Daemon {
        /// The config it runs with.
        running: String,
        /// When auto sizing last changed each profile, in seconds, as it made the changes.
        last: BTreeMap<String, u64>,
        /// Each profile's `cpus` as it announced them.
        cpus_set: Vec<CpusSet>,
    }

    /// A host: its config file, the store, the daemon, and the oracle's record of the transitions it accepted.
    struct World {
        /// The config file on disk, which hand edits change before a reload or a restart runs it.
        file: String,
        rules: String,
        slice: Option<u64>,
        store: Stored,
        daemon: Daemon,
        /// The oracle: from when, in ms, runs at each profile's `cpus` are fresh, set by each accepted transition:
        /// the daemon running other `cpus`, or auto sizing changing the profile.
        fresh: BTreeMap<String, u64>,
        /// The oracle: when auto sizing last changed each profile, in seconds.
        changed: BTreeMap<String, u64>,
        /// What each profile's last change replaced, for an undo.
        undo: BTreeMap<String, Resize>,
        /// How many lowerings of `cpus` the looks made, so the sequences are known to try them.
        lowered: u64,
        now: u64,
        log: String,
    }

    impl World {
        fn config(&self) -> Config {
            on(&self.daemon.running, &self.rules)
        }

        /// The last 30 days, as the store reads them.
        fn history(&self) -> History {
            let since = self.now - 30 * DAY;
            let recent = |runs: &[Ran]| -> Vec<Ran> {
                runs.iter()
                    .filter(|job| job.queued_at >= since)
                    .cloned()
                    .collect()
            };
            History {
                jobs: recent(&self.store.jobs),
                cancelled: recent(&self.store.cancelled),
                ..History::default()
            }
        }

        fn look(&self) -> Look {
            look_set(
                &self.config(),
                &self.history(),
                self.slice,
                self.now,
                &self.daemon.last,
                &self.daemon.cpus_set,
            )
        }

        /// Runs `text`, as a reload, a restart or a write of the config does: the profiles whose `cpus` it changes
        /// start their evidence over, and the daemon announces each profile whose `cpus` differ from its latest event.
        fn run_with(&mut self, text: String) {
            let before = self.config();
            self.daemon.running = text;
            let after = self.config();
            for (name, profile) in &after.profiles {
                if before.profiles[name].cpus != profile.cpus {
                    self.fresh.insert(name.clone(), self.now);
                }
                let latest = self
                    .store
                    .events
                    .iter()
                    .rfind(|event| event.profile == *name)
                    .map(|event| event.cpus);
                if latest != Some(profile.cpus) {
                    let event = CpusSet {
                        profile: name.clone(),
                        cpus: profile.cpus,
                        at: self.now,
                    };
                    self.store.events.push(event.clone());
                    self.daemon.cpus_set.push(event);
                }
            }
        }

        /// Writes `to` into the profile, on disk and in what the daemon runs, as `PUT /v1/config` does.
        fn write(&mut self, profile: &str, to: Resize) {
            self.file = resize_profile(&self.file, profile, to).unwrap();
            self.run_with(self.file.clone());
        }

        /// Restarts the daemon: it reads the file and rebuilds the rest from the store. Run with the same file, it
        /// decides as it did before; `Err` when it does not.
        fn restart(&mut self) -> Result<(), String> {
            let unchanged = self.file == self.daemon.running;
            let before = unchanged.then(|| self.look());
            let mut last = BTreeMap::new();
            for (profile, at) in &self.store.changes {
                let latest = last.entry(profile.clone()).or_insert(*at);
                *latest = (*latest).max(*at);
            }
            self.daemon.last = last;
            self.daemon.cpus_set = self.store.events.clone();
            self.run_with(self.file.clone());
            match before {
                Some(before) if before != self.look() => {
                    Err("a restart decides otherwise".to_owned())
                }
                _ => Ok(()),
            }
        }

        /// Runs of one of `profile`'s jobs at its cap now, some failed, cancelled or unmeasured. A run that bursts
        /// past the cap is throttled in a share of its busy periods; one on a contended host, whose other programs
        /// hold its CPUs, gets less than it asks without its quota throttling it. Some have no throttling figure,
        /// as runs from before schema 17 or on a kernel without the cpu controller.
        fn run(&mut self, rng: &mut Rng, profile: &str) {
            let config = self.config();
            let limits = &config.profiles[profile];
            let cap = u64::from(limits.cpus) * 1000;
            let (name, heavy, wide) = rng.pick(&JOBS);
            let contended = rng.below(4) == 0;
            for _ in 0..=rng.below(5) {
                let share = heavy.saturating_sub(10) + rng.below(21);
                let wants = wide * 1000 - rng.below(1000);
                let mut gets = wants.min(cap);
                // Held at the cap, its CPU-bound part takes longer.
                let took = 300_000 + 600_000 * wants / gets.max(1);
                let wall = took / 100;
                let mut throttled = if wants > cap {
                    // The longer it bursts past the cap, the more of its run the quota holds.
                    wall * (wants - cap).min(cap) / cap / 2 + rng.below(wall / 20)
                } else {
                    rng.below(wall / 50)
                };
                let mut periods = wall;
                if rng.below(5) == 0 {
                    // Idle between a few bursts, as a job waiting on the network: the kernel counts only the busy
                    // periods, and throttles most of them.
                    periods = wall / 6;
                    throttled = rng.below(periods);
                }
                if contended {
                    gets = gets * (50 + rng.below(40)) / 100;
                    throttled = 0;
                }
                let started = self.now + rng.below(300) * 1000;
                let mut ran = Ran {
                    profile: profile.to_owned(),
                    memory_cap: Some(limits.memory.0),
                    memory_peak: limits.memory.0 / 4 + rng.below(limits.memory.0 / 2),
                    cpu_cap: Some(cap),
                    cpu_peak: gets,
                    cpu_average: Some((gets * share / 100).max(1)),
                    cpu_periods: Some(periods),
                    cpu_throttled: Some(throttled),
                    queued_at: self.now,
                    started_at: Some(started),
                    runner_started_at: self.now,
                    finished_at: Some(started + took),
                    ..job("halyard/ledger", name, profile, 0, 0)
                };
                if rng.below(4) == 0 {
                    (ran.cpu_periods, ran.cpu_throttled) = (None, None);
                }
                match rng.below(20) {
                    0 | 1 => ran.succeeded = false,
                    2 => ran.cpu_average = None,
                    3 => ran.cpu_cap = None,
                    4 => ran.cpu_average = Some(0),
                    5 | 6 => {
                        self.store.cancelled.push(ran);
                        continue;
                    }
                    _ => {}
                }
                self.store.jobs.push(ran);
            }
            self.now += 1000 * rng.below(600);
        }
    }

    /// What `cpus` of `profile` at `cap` (ms) may move on, by job: successful runs at the cap since the oracle's
    /// last transition of the profile, each job's measured averages. A job without one is listed with none.
    fn evidence(world: &World, profile: &str, cap: u64) -> BTreeMap<&'static str, Vec<u64>> {
        let since = world
            .fresh
            .get(profile)
            .copied()
            .unwrap_or(0)
            .max(world.now - 30 * DAY);
        let mut jobs: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for job in world.history().jobs.iter().filter(|job| {
            job.profile == profile
                && job.cpu_cap == Some(cap)
                && job.succeeded
                && job.queued_at >= since
        }) {
            let name = JOBS.iter().find(|(name, ..)| *name == job.name).unwrap().0;
            jobs.entry(name)
                .or_default()
                .extend(job.cpu_average.filter(|average| *average > 0));
        }
        jobs
    }

    /// Whether the cap held a run back: its quota throttled it in 15% of its run's 100 ms periods, or, without that
    /// figure, its burst met the cap.
    fn held(job: &Ran, cap: u64) -> bool {
        match job.cpu_periods.zip(job.cpu_throttled) {
            Some((periods, throttled)) => {
                let run = (job.finished_at.unwrap() - job.started_at.unwrap()) / 100;
                throttled > 0 && throttled * 100 >= run.max(periods) * 15
            }
            None => job.cpu_peak * 100 >= cap * 95,
        }
    }

    /// How many of the runs `evidence` reads the cap held back, by job.
    fn holds(world: &World, profile: &str, cap: u64) -> BTreeMap<String, usize> {
        let since = world
            .fresh
            .get(profile)
            .copied()
            .unwrap_or(0)
            .max(world.now - 30 * DAY);
        let mut jobs: BTreeMap<String, usize> = BTreeMap::new();
        for job in world.history().jobs.iter().filter(|job| {
            job.profile == profile
                && job.cpu_cap == Some(cap)
                && job.succeeded
                && job.queued_at >= since
        }) {
            *jobs.entry(job.name.clone()).or_default() += usize::from(held(job, cap));
        }
        jobs
    }

    /// The CPU peaks of the runs `evidence` reads, by job.
    fn peaks(world: &World, profile: &str, cap: u64) -> BTreeMap<String, Vec<u64>> {
        let since = world
            .fresh
            .get(profile)
            .copied()
            .unwrap_or(0)
            .max(world.now - 30 * DAY);
        let mut jobs: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for job in world.history().jobs.iter().filter(|job| {
            job.profile == profile
                && job.cpu_cap == Some(cap)
                && job.succeeded
                && job.queued_at >= since
        }) {
            jobs.entry(job.name.clone()).or_default().push(job.cpu_peak);
        }
        jobs
    }

    /// The highest CPU peak of the runs `evidence` reads.
    fn peak(world: &World, profile: &str, cap: u64) -> u64 {
        peaks(world, profile, cap)
            .values()
            .flatten()
            .copied()
            .max()
            .unwrap_or(0)
    }

    /// The CPUs every profile's slots take, millicores.
    fn taken(config: &Config) -> u64 {
        config
            .profiles
            .values()
            .map(|profile| u64::from(profile.cpus) * 1000 * u64::from(profile.slots))
            .sum()
    }

    /// Whether the evidence allows a raise of `cpus`: 3 runs of a job averaging 70% of the cap at the median, or under
    /// measured admission, 3 runs of a job the cap held back.
    fn raisable(
        world: &World,
        config: &Config,
        profile: &str,
        cap: u64,
        jobs: &BTreeMap<&'static str, Vec<u64>>,
    ) -> bool {
        let bound = jobs
            .values()
            .any(|averages| averages.len() >= 3 && spread(averages.clone()).p50 * 100 >= cap * 70);
        let held = config.host.admission == Admission::Measured
            && holds(world, profile, cap).values().any(|held| *held >= 3);
        bound || held
    }

    /// Checks a look's `cpus` changes against the evidence and the rules; `Err` says what broke.
    fn check(world: &World, look: &Look) -> Result<(), String> {
        let config = world.config();
        let rules = &config.sizing;
        for change in &look.changes {
            let SizingChange {
                profile, from, to, ..
            } = change;
            let (Some(from_cpus), Some(to_cpus)) = (from.cpus, to.cpus) else {
                continue;
            };
            let current = config.profiles[profile].cpus;
            if from_cpus != current {
                return Err(format!("{profile}: from {from_cpus} CPUs, has {current}"));
            }
            let cap = u64::from(current) * 1000;
            let jobs = evidence(world, profile, cap);
            let since = world
                .changed
                .get(profile)
                .map_or(u64::MAX, |at| world.now / 1000 - at);
            let step = (current * rules.max_step / 100).max(1);
            if to_cpus == 0 || current.abs_diff(to_cpus) > step {
                return Err(format!(
                    "{profile}: {current} → {to_cpus} CPUs, more than a step"
                ));
            }
            if to_cpus > current {
                if !raisable(world, &config, profile, cap, &jobs) {
                    return Err(format!(
                        "{profile}: raised without a job at its cap: {jobs:?}"
                    ));
                }
                if world
                    .slice
                    .is_none_or(|slice| u64::from(to_cpus) * 1000 > slice)
                {
                    return Err(format!("{profile}: raised past the slice's CPUs"));
                }
                if since < rules.raise_after().as_secs() {
                    return Err(format!("{profile}: raised {since}s after its last change"));
                }
            } else {
                if config.host.admission == Admission::Measured {
                    return Err(format!("{profile}: lowered under measured admission"));
                }
                let peak = peak(world, profile, cap);
                let held: usize = holds(world, profile, cap).values().sum();
                if held > 0 {
                    return Err(format!("{profile}: lowered on {held} runs held"));
                }
                if u64::from(to_cpus) * 1000 < peak {
                    return Err(format!(
                        "{profile}: lowered to {to_cpus} CPUs under the peak {peak}"
                    ));
                }
                let runs: usize = jobs.values().map(Vec::len).sum();
                if runs < rules.min_jobs as usize {
                    return Err(format!("{profile}: lowered on {runs} runs"));
                }
                if jobs.values().any(Vec::is_empty) {
                    return Err(format!(
                        "{profile}: lowered past a job without a measured average: {jobs:?}"
                    ));
                }
                let heaviest = jobs
                    .values()
                    .map(|averages| spread(averages.clone()).p95)
                    .max()
                    .unwrap_or(u64::MAX);
                if u64::from(to_cpus) * 1000 < heaviest * 2 {
                    return Err(format!(
                        "{profile}: lowered to {to_cpus} CPUs under twice its heaviest job's {heaviest}"
                    ));
                }
                if since < rules.interval.as_secs() {
                    return Err(format!("{profile}: lowered {since}s after its last change"));
                }
            }
        }
        if let (Some(overcommit), Some(slice)) = (config.host.cpu_overcommit, world.slice) {
            let mut after = world.daemon.running.clone();
            for change in &look.changes {
                after = resize_profile(&after, &change.profile, change.to).unwrap();
            }
            let (held, what) = match config.host.admission {
                Admission::Caps => (taken as fn(&Config) -> u64, "slots × cpus"),
                Admission::Measured => (
                    at_caps as fn(&Config) -> u64,
                    "warm runners and one more at caps",
                ),
            };
            let (before, after) = (held(&config), held(&on(&after, &world.rules)));
            let budget = cpu_budget(slice, overcommit);
            if after > before.max(budget) {
                return Err(format!(
                    "{what} {before} → {after} past the budget {budget}"
                ));
            }
        }
        Ok(())
    }

    /// What measured admission holds at caps, millicores: every profile's warm runners idle at their caps, and one
    /// more runner at the largest cap.
    fn at_caps(config: &Config) -> u64 {
        let warm: u64 = config
            .profiles
            .iter()
            .map(|(name, profile)| {
                let pools: u32 = config
                    .repositories
                    .iter()
                    .filter(|repository| repository.profile == *name)
                    .map(|repository| repository.warm)
                    .sum();
                u64::from(pools.min(profile.slots)) * u64::from(profile.cpus)
            })
            .sum();
        let largest = config
            .profiles
            .values()
            .map(|profile| profile.cpus)
            .max()
            .unwrap_or(0);
        (warm + u64::from(largest)) * 1000
    }

    /// Looks, checks what the look picks and applies it, as the daemon does every 5 minutes; nothing while the file
    /// on disk differs from what the daemon runs.
    fn look_and_apply(world: &mut World) -> Result<(), String> {
        if world.file != world.daemon.running {
            let _ = writeln!(world.log, "look: blocked by the file on disk");
            return Ok(());
        }
        let look = world.look();
        check(world, &look)?;
        let config = world.config();
        for change in look.changes {
            let _ = writeln!(world.log, "look: {} {}", change.profile, change.line());
            if change.to.cpus < change.from.cpus {
                world.lowered += 1;
            }
            let profile = &config.profiles[&change.profile];
            world.undo.insert(
                change.profile.clone(),
                Resize {
                    cpus: Some(profile.cpus),
                    ..change.from
                },
            );
            world
                .store
                .changes
                .push((change.profile.clone(), change.at));
            world.daemon.last.insert(change.profile.clone(), change.at);
            world.changed.insert(change.profile.clone(), change.at);
            world.fresh.insert(change.profile.clone(), world.now);
            world.write(&change.profile, change.to);
        }
        Ok(())
    }

    impl World {
        /// A host with a config and rules `rng` picks, the daemon not yet started, and the text it starts with.
        fn started(rng: &mut Rng) -> (World, String) {
            let budget = rng.below(2) == 0;
            let measured = rng.below(3) == 0;
            let host = |set: bool, line: &str| {
                if set {
                    format!("\n{line}")
                } else {
                    String::new()
                }
            };
            let text = CONFIG.replace(
                "isolation = \"sysbox\"",
                &format!(
                    "isolation = \"sysbox\"{}{}",
                    host(budget, "cpu_overcommit = 1.0"),
                    host(measured, "admission = \"measured\"")
                ),
            );
            let mut world = World {
                file: text.clone(),
                rules: rng
                    .pick(&["", "min_jobs = 3", "max_step = 50\nmin_jobs = 5"])
                    .to_owned(),
                slice: rng.pick(&[Some(32_000), Some(48_000), None]),
                store: Stored {
                    jobs: Vec::new(),
                    cancelled: Vec::new(),
                    changes: Vec::new(),
                    events: Vec::new(),
                },
                daemon: Daemon {
                    running: text.clone(),
                    last: BTreeMap::new(),
                    cpus_set: Vec::new(),
                },
                fresh: BTreeMap::new(),
                changed: BTreeMap::new(),
                undo: BTreeMap::new(),
                lowered: 0,
                now: NOW,
                log: String::new(),
            };
            let _ = writeln!(
                world.log,
                "budget {budget}, measured {measured}, rules {:?}, slice {:?}",
                world.rules, world.slice
            );
            (world, text)
        }
    }

    /// One generated sequence; `Ok` with how many lowerings of `cpus` it made.
    fn case(seed: u64) -> Result<u64, String> {
        let mut rng = Rng(seed);
        let (mut world, text) = World::started(&mut rng);
        // The daemon starts: it announces every profile.
        world.run_with(text);
        for _ in 0..EVENTS {
            let step = match rng.below(12) {
                0..=3 => {
                    let profile = rng.pick(&PROFILES);
                    world.run(&mut rng, profile);
                    let _ = writeln!(world.log, "runs of {profile}");
                    Ok(())
                }
                4 | 5 => {
                    let hours = rng.pick(&[1, 2, 25, 72]);
                    world.now += hours * HOUR;
                    let _ = writeln!(world.log, "{hours}h pass");
                    Ok(())
                }
                6 | 7 => look_and_apply(&mut world),
                8 => {
                    // A hand edit of `cpus`, run at once as the dashboard's editor does, or left on disk.
                    let profile = rng.pick(&PROFILES);
                    let limits = world.config().profiles[profile].clone();
                    let cpus = u32::try_from(1 + rng.below(32)).unwrap();
                    let to = Resize {
                        memory: limits.memory.0,
                        slots: limits.slots,
                        cpus: Some(cpus),
                    };
                    world.file = resize_profile(&world.file, profile, to).unwrap();
                    let reloaded = rng.below(2) == 0;
                    if reloaded {
                        world.run_with(world.file.clone());
                    }
                    let _ = writeln!(
                        world.log,
                        "hand edit: {profile} cpus {cpus}, reloaded {reloaded}"
                    );
                    Ok(())
                }
                9 => {
                    world.run_with(world.file.clone());
                    let _ = writeln!(world.log, "reload");
                    Ok(())
                }
                10 => {
                    let _ = writeln!(world.log, "restart");
                    world.restart()
                }
                _ => {
                    let profile = rng.pick(&PROFILES);
                    if world.file == world.daemon.running
                        && let Some(from) = world.undo.remove(profile)
                    {
                        world.write(profile, from);
                        let _ = writeln!(world.log, "undo {profile}: {from:?}");
                    }
                    Ok(())
                }
            };
            step.map_err(|broke| format!("{}{broke}", world.log))?;
        }
        Ok(world.lowered)
    }

    #[test]
    fn cpus_move_only_on_fresh_evidence_within_the_budget_and_a_step_a_window() {
        let env = |name| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
        };
        let seeds: Vec<u64> = if let Some(seed) = env("RUNNERD_INVARIANT_SEED") {
            vec![seed]
        } else {
            (0..env("RUNNERD_INVARIANT_CASES").unwrap_or(CASES)).collect()
        };
        let mut lowered = 0;
        for seed in &seeds {
            match case(*seed) {
                Ok(count) => lowered += count,
                Err(broke) => panic!("seed {seed}:\n{broke}"),
            }
        }
        assert!(
            seeds.len() < 8 || lowered > 0,
            "the sequences lower cpus somewhere"
        );
    }
}
