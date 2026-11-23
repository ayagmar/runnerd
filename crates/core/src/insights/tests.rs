//! Insights from synthetic histories of fictional repositories: one `halyard/api` job run a few times a day.

use std::collections::BTreeMap;

use super::{HeldJob, History, HostHold, HostReading, Run, digest, insights};
use crate::api::{self, AutoNote, InsightKind, JobState, Severity};
use crate::config::Config;

const G: u64 = 1 << 30;
const MINUTE: u64 = 60 * 1000;
const HOUR: u64 = 60 * MINUTE;
const NOW: u64 = 1_800_000_000_000;

const CONFIG: &str = r#"
[host]
slice = "runnerd.slice"
isolation = "sysbox"
admission = "caps"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.app]
image_stage = "node"
cpus = 4
memory = "4G"
slots = 2

[[repositories]]
name = "halyard/api"
profile = "app"
labels = ["app"]
"#;

fn config() -> Config {
    Config::parse(CONFIG).unwrap()
}

/// The `n`-th run of `halyard/api`'s `CI / test` on main, `hours_ago` hours before `NOW`, taking `seconds`, and
/// succeeding.
fn test_run(n: u64, hours_ago: u64, seconds: u64) -> Run {
    let finished = NOW - hours_ago * HOUR;
    Run {
        id: 1000 + n,
        repository: "halyard/api".to_owned(),
        workflow: Some("CI".to_owned()),
        name: "test".to_owned(),
        branch: Some("main".to_owned()),
        url: Some(format!(
            "https://github.com/halyard/api/actions/runs/{n}/job/{}",
            1000 + n
        )),
        run_id: n,
        run_attempt: 1,
        state: JobState::Completed,
        conclusion: Some("success".to_owned()),
        queued_at: finished - seconds * 1000 - 20_000,
        started_at: Some(finished - seconds * 1000),
        finished_at: Some(finished),
        runner_started_at: Some(finished - seconds * 1000 - 15_000),
        profile: Some("app".to_owned()),
        memory_peak: Some(G),
        build_cache: None,
        sock_throttled: Some(0),
        unseen: 0,
        cpu_cap: Some(4000),
        cpu_periods: None,
        cpu_throttled: None,
        disk: None,
        seen_at: Some(finished - seconds * 1000 - 15_000),
        unseen_unlisted: 0,
        slept: false,
        docker_store: None,
        causes: Some(BTreeMap::new()),
    }
}

/// `runs` in a history that holds every job however old.
fn whole(runs: &[Run]) -> History {
    History {
        runs: runs.to_vec(),
        kept_since: 0,
        host: None,
        readings: Vec::new(),
        disk: Vec::new(),
        drive_read: None,
    }
}

/// Runs oldest first, one every 6 hours, ending an hour ago, taking the given seconds.
fn history(durations: &[u64]) -> Vec<Run> {
    let count = durations.len() as u64;
    durations
        .iter()
        .zip(0..)
        .map(|(seconds, n)| test_run(n, 1 + (count - 1 - n) * 6, *seconds))
        .collect()
}

#[test]
fn a_job_that_got_slower_and_stayed_slower_is_slower_since_its_first_slow_run() {
    // Eight runs at 4 minutes, then six at 6: the first 6-minute run is the 9th.
    let mut durations = vec![240; 8];
    durations.extend([360; 6]);
    let runs = history(&durations);

    let found = insights(&whole(&runs), &config(), 14, NOW).insights;

    assert_eq!(found.len(), 1, "{found:#?}");
    let slower = &found[0];
    assert_eq!(slower.key, "slower:halyard/api/CI/test");
    assert_eq!(slower.severity, Severity::Warning);
    let InsightKind::Slower {
        branch,
        before,
        now,
        since,
    } = &slower.kind
    else {
        panic!("not slower: {slower:#?}");
    };
    assert_eq!((branch.as_deref(), *before, *now), (Some("main"), 240, 360));
    assert_eq!(since.as_ref().map(|since| since.job), Some(runs[8].id));
    assert_eq!(
        slower.sentence,
        "halyard/api: CI / test on main takes 6m now, up from 4m."
    );
}

#[test]
fn a_one_off_slow_run_is_not_a_regression() {
    let mut durations = vec![240; 13];
    durations[10] = 900;

    assert!(
        insights(&whole(&history(&durations)), &config(), 14, NOW)
            .insights
            .is_empty(),
        "one slow run among fast ones says nothing"
    );
}

#[test]
fn a_job_fast_again_in_its_last_two_runs_is_not_slower() {
    let mut durations = vec![240; 8];
    durations.extend([360, 360, 360, 240, 240]);

    assert!(
        insights(&whole(&history(&durations)), &config(), 14, NOW)
            .insights
            .is_empty(),
        "the slow stretch ended"
    );
}

#[test]
fn a_job_that_flips_between_cache_hits_and_misses_is_not_slower() {
    // A build cache hit takes about 40 s and a miss about 2 minutes; more misses lately move the median, not the job.
    let durations = [
        112, 37, 44, 46, 146, 39, 124, 134, 41, 156, 122, 126, 147, 40,
    ];

    assert!(
        insights(&whole(&history(&durations)), &config(), 14, NOW)
            .insights
            .is_empty(),
        "a recent run as fast as before says the job did not get slower"
    );
}

#[test]
fn a_small_slowdown_is_not_a_regression() {
    // 20% up but only 24 seconds.
    let mut durations = vec![120; 8];
    durations.extend([144; 6]);

    assert!(
        insights(&whole(&history(&durations)), &config(), 14, NOW)
            .insights
            .is_empty(),
        "under 30 seconds is noise"
    );
}

#[test]
fn slower_runs_on_a_branch_with_few_runs_do_not_count() {
    let mut runs = history(&[240; 10]);
    for n in 0..6 {
        let mut feature = test_run(100 + n, 2, 600);
        feature.branch = Some("feature/slow".to_owned());
        runs.push(feature);
    }

    assert!(
        insights(&whole(&runs), &config(), 14, NOW)
            .insights
            .is_empty(),
        "only the busiest branch is compared"
    );
}

#[test]
fn main_is_compared_even_when_pull_requests_run_more() {
    let mut runs = history(&[240; 10]);
    // A pull request that adds tests, pushed often: slower, and busier than main.
    for n in 0..14 {
        let mut feature = test_run(100 + n, 30 - n, if n < 7 { 240 } else { 600 });
        feature.branch = Some("feature/more-tests".to_owned());
        runs.push(feature);
    }

    assert!(
        insights(&whole(&runs), &config(), 14, NOW)
            .insights
            .is_empty(),
        "main runs as fast as it did"
    );
}

/// Attempt `attempt` of run `run_id` of `CI / test`, concluding `conclusion`.
fn attempt(run_id: u64, attempt: u32, conclusion: &str) -> Run {
    let mut run = test_run(run_id * 10 + u64::from(attempt), 3, 240);
    run.run_id = run_id;
    run.run_attempt = attempt;
    run.conclusion = Some(conclusion.to_owned());
    run
}

#[test]
fn a_job_that_failed_and_passed_on_a_re_run_is_flaky() {
    let runs = vec![
        attempt(1, 1, "success"),
        attempt(2, 1, "failure"),
        attempt(2, 2, "success"),
        attempt(3, 1, "success"),
    ];

    let found = insights(&whole(&runs), &config(), 14, NOW).insights;

    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].key, "flaky:halyard/api/CI/test");
    assert_eq!(found[0].kind, InsightKind::Flaky { flaky: 1, total: 3 });
    assert_eq!(
        found[0].severity,
        Severity::Notice,
        "once may be chance: shown, not sent"
    );
    assert_eq!(found[0].runs[0].job, runs[1].id, "links the failed attempt");
    assert_eq!(
        found[0].sentence,
        "halyard/api: CI / test failed and then passed on a re-run in 1 of its 3 runs."
    );
}

#[test]
fn a_job_flaky_twice_is_a_warning() {
    let runs = vec![
        attempt(1, 1, "failure"),
        attempt(1, 2, "success"),
        attempt(2, 1, "timed_out"),
        attempt(2, 2, "success"),
    ];

    let found = insights(&whole(&runs), &config(), 14, NOW).insights;

    assert_eq!(found[0].kind, InsightKind::Flaky { flaky: 2, total: 2 });
    assert_eq!(found[0].severity, Severity::Warning);
}

#[test]
fn a_job_that_failed_again_on_a_re_run_is_not_flaky() {
    let runs = vec![attempt(2, 1, "failure"), attempt(2, 2, "failure")];

    assert!(
        insights(&whole(&runs), &config(), 14, NOW)
            .insights
            .is_empty(),
        "failing twice is broken, not flaky"
    );
}

#[test]
fn a_lost_job_that_passed_on_a_re_run_is_not_flaky() {
    let mut lost = attempt(2, 1, "failure");
    lost.state = JobState::Lost;
    lost.conclusion = None;
    let runs = vec![lost, attempt(2, 2, "success")];

    assert!(
        insights(&whole(&runs), &config(), 14, NOW)
            .insights
            .is_empty(),
        "a vanished runner is the host's fault"
    );
}

// Catches a failure the host's sleep caused read as the code's: GitHub ended the runner's session, and the re-run
// passed (ADR 0159).
#[test]
fn a_job_that_failed_because_the_host_slept_and_passed_on_a_re_run_is_not_flaky() {
    let mut slept = attempt(2, 1, "failure");
    slept.slept = true;
    let runs = vec![slept, attempt(2, 2, "success")];

    assert!(
        insights(&whole(&runs), &config(), 14, NOW)
            .insights
            .is_empty(),
        "a sleep of the host is not the job's fault"
    );
}

// Catches the hours a host slept counted in its jobs' run times, so the runs across a sleep read as a regression.
#[test]
fn runs_that_spanned_a_sleep_of_the_host_are_not_slower() {
    let mut durations = vec![240; 8];
    durations.extend([3_840; 6]);
    let mut runs = history(&durations);
    for run in &mut runs[8..] {
        run.slept = true;
    }

    assert!(
        insights(&whole(&runs), &config(), 14, NOW)
            .insights
            .is_empty(),
        "the sleep is not the job's run time"
    );
}

/// Five runs on `branch` whose runners found `hits` of 40 lookups each in the build cache, and could write to it
/// or not.
fn cached(branch: &str, hits: u64, writer: bool) -> Vec<Run> {
    history(&[240; 5])
        .into_iter()
        .map(|run| Run {
            branch: Some(branch.to_owned()),
            build_cache: Some(api::BuildCacheCounts {
                hits,
                misses: 40 - hits,
                writer,
                ..api::BuildCacheCounts::default()
            }),
            ..run
        })
        .collect()
}

#[test]
fn main_runs_that_find_nothing_their_predecessors_stored_have_a_cold_cache() {
    let found = insights(&whole(&cached("main", 1, true)), &config(), 14, NOW).insights;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].kind,
        InsightKind::ColdCache {
            branch: Some("main".to_owned()),
            hits: 5,
            lookups: 200,
        }
    );
    assert!(
        found[0].sentence.contains("2% of its build cache lookups"),
        "{}",
        found[0].sentence
    );
}

#[test]
fn a_warm_cache_or_runs_that_only_read_are_no_insight() {
    for runs in [cached("main", 30, true), cached("feature", 0, false)] {
        let found = insights(&whole(&runs), &config(), 14, NOW).insights;
        assert!(found.is_empty(), "{found:?}");
    }
}

/// Ten runs, those at `throttled` with their sockets throttled for memory pressure.
fn throttled_at(throttled: &[usize]) -> Vec<Run> {
    let mut runs = history(&[240; 10]);
    for at in throttled {
        runs[*at].sock_throttled = Some(846);
    }
    runs
}

#[test]
fn downloads_memory_pressure_throttled_in_several_runs_are_a_warning() {
    let found = insights(&whole(&throttled_at(&[3, 8])), &config(), 14, NOW).insights;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(
        found[0].kind,
        InsightKind::Throttled {
            throttled: 2,
            total: 10
        }
    );
    let linked: Vec<u64> = found[0].runs.iter().map(|run| run.job).collect();
    assert_eq!(linked, [1008, 1003], "the throttled runs, newest first");
}

#[test]
fn one_throttled_run_is_no_insight() {
    let found = insights(&whole(&throttled_at(&[5])), &config(), 14, NOW).insights;
    assert!(found.is_empty(), "{found:?}");
}

fn peaking(peaks_in_tenths_of_g: &[u64]) -> Vec<Run> {
    let mut runs = history(&vec![240; peaks_in_tenths_of_g.len()]);
    for (run, peak) in runs.iter_mut().zip(peaks_in_tenths_of_g) {
        run.memory_peak = Some(peak * G / 10);
    }
    runs
}

#[test]
fn memory_near_the_cap_is_flagged_with_what_auto_sizing_does() {
    // 3.6G of a 4G cap: 90%.
    let runs = peaking(&[30, 31, 33, 35, 36, 36]);

    let found = insights(&whole(&runs), &config(), 14, NOW).insights;

    assert_eq!(found.len(), 1, "{found:#?}");
    let InsightKind::NearCap {
        profile, cap, auto, ..
    } = &found[0].kind
    else {
        panic!("not near the cap: {found:#?}");
    };
    assert_eq!(
        (profile.as_str(), *cap, *auto),
        ("app", 4 * G, AutoNote::Off)
    );
    assert_eq!(
        found[0].severity,
        Severity::Warning,
        "three runs near the cap and nothing raises it"
    );
    assert!(
        found[0]
            .sentence
            .ends_with("Auto sizing is off: raise app's memory, or turn it on."),
        "{}",
        found[0].sentence
    );

    let auto = Config::parse(&format!("{CONFIG}\n[sizing]\nauto = true\n")).unwrap();
    let found = insights(&whole(&runs), &auto, 14, NOW).insights;
    assert!(
        matches!(
            found[0].kind,
            InsightKind::NearCap {
                auto: AutoNote::Raises,
                ..
            }
        ),
        "auto sizing raises it: {found:#?}"
    );
    assert_eq!(
        found[0].severity,
        Severity::Notice,
        "auto sizing deals with it: shown, not sent"
    );
}

#[test]
fn one_spike_near_the_cap_is_a_notice() {
    // With six runs the 95th percentile is the highest: 3.6G of 4G once.
    let found = insights(
        &whole(&peaking(&[30, 30, 31, 30, 30, 36])),
        &config(),
        14,
        NOW,
    )
    .insights;

    assert!(
        matches!(found[0].kind, InsightKind::NearCap { .. }),
        "{found:#?}"
    );
    assert_eq!(found[0].severity, Severity::Notice);
}

#[test]
fn memory_well_under_the_cap_says_nothing() {
    assert!(
        insights(
            &whole(&peaking(&[20, 21, 22, 20, 24, 25])),
            &config(),
            14,
            NOW
        )
        .insights
        .is_empty(),
        "62% of the cap is fine"
    );
}

#[test]
fn a_job_served_in_two_profiles_is_near_the_cap_by_its_last_profiles_runs_alone() {
    let config = Config::parse(&format!(
        "{CONFIG}\n[profiles.light]\nimage_stage = \"node\"\ncpus = 1\nmemory = \"1G\"\nslots = 1\n"
    ))
    .unwrap();
    // Nine runs of 3G in app, 75% of its 4G, then one of 200M in light: light has one small run, not 300% of its cap.
    let mut runs = peaking(&[30, 30, 30, 30, 30, 30, 30, 30, 30, 2]);
    runs.last_mut().unwrap().profile = Some("light".to_owned());

    assert!(
        insights(&whole(&runs), &config, 14, NOW)
            .insights
            .is_empty(),
        "nothing is near a cap"
    );
}

/// Runs of 240 s, one every 6 hours, each its quota throttled in the given share of its run, per mille, or without
/// a throttling figure (`None`), at app's 4 CPUs.
fn throttled_runs(shares: &[Option<u64>]) -> Vec<Run> {
    let mut runs = history(&vec![240; shares.len()]);
    for (run, share) in runs.iter_mut().zip(shares) {
        // 240 s is 2400 periods of 100 ms, all of them busy.
        run.cpu_periods = share.map(|_| 2400);
        run.cpu_throttled = share.map(|share| share * 2400 / 1000);
    }
    runs
}

fn measured(auto: bool) -> Config {
    Config::parse(&format!(
        "{}\n[sizing]\nauto = {auto}\n",
        CONFIG.replace("admission = \"caps\"", "admission = \"measured\"")
    ))
    .unwrap()
}

fn cpu_held(history: &History, config: &Config) -> Vec<api::Insight> {
    insights(history, config, 14, NOW)
        .insights
        .into_iter()
        .filter(|insight| matches!(insight.kind, InsightKind::CpuHeld { .. }))
        .collect()
}

#[test]
fn a_job_its_cpu_cap_held_back_three_times_says_the_cpus_to_set_under_caps() {
    // Throttled in exactly 15% of their run twice and in 30% once: held, as sizing reads them. 14.9% is not.
    let runs = throttled_runs(&[
        Some(0),
        Some(150),
        Some(149),
        Some(40),
        Some(300),
        Some(150),
        Some(0),
    ]);

    let found = cpu_held(&whole(&runs), &config());

    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].key, "cpu-held:halyard/api/CI/test");
    assert_eq!(found[0].severity, Severity::Warning, "the owner acts");
    assert_eq!(
        found[0].kind,
        InsightKind::CpuHeld {
            profile: "app".to_owned(),
            cpus: 4,
            held: 3,
            total: 7,
            raised_from: None,
            set: Some(6),
        }
    );
    assert_eq!(
        found[0].sentence,
        "halyard/api: CI / test was held back by app's 4 CPUs in 3 of its last 7 runs, its quota throttling it in 15% \
         of its run or more. Under caps admission each CPU takes budget, so auto sizing does not raise cpus for this: \
         set app's cpus to 6 if runnerctl sizing shows the budget has room."
    );
    let linked: Vec<u64> = found[0].runs.iter().map(|run| run.job).collect();
    assert_eq!(linked, [1005, 1004, 1001], "the held runs, newest first");

    // The profile has other cpus now: these runs say nothing of them.
    let raised = Config::parse(&CONFIG.replace("cpus = 4", "cpus = 6")).unwrap();
    assert!(
        cpu_held(&whole(&runs), &raised).is_empty(),
        "runs at 4 CPUs say nothing of 6"
    );
}

#[test]
fn runs_without_a_throttling_figure_are_neither_held_nor_free() {
    let unknown = [None; 8];
    assert!(
        cpu_held(&whole(&[]), &config()).is_empty(),
        "no runs say nothing"
    );
    assert!(
        cpu_held(&whole(&throttled_runs(&unknown)), &config()).is_empty(),
        "runs from before schema 17 say nothing"
    );
    // Two held runs are under the three sizing raises on, however few runs were measured.
    let mut shares = unknown.to_vec();
    shares.extend([Some(200), Some(200)]);
    assert!(
        cpu_held(&whole(&throttled_runs(&shares)), &config()).is_empty(),
        "two held runs are no insight"
    );
    // A third, among runs without the figure: 3 of the 3 measured, the rest not counted either way.
    shares.insert(3, Some(200));
    let found = cpu_held(&whole(&throttled_runs(&shares)), &config());
    assert!(
        matches!(
            found[0].kind,
            InsightKind::CpuHeld {
                held: 3,
                total: 3,
                ..
            }
        ),
        "{found:#?}"
    );
}

#[test]
fn under_measured_admission_auto_sizing_raises_cpus_until_a_raise_does_not_help() {
    let held = throttled_runs(&[Some(300); 4]);

    let found = cpu_held(&whole(&held), &measured(true));
    assert_eq!(
        found[0].severity,
        Severity::Notice,
        "auto sizing deals with it"
    );
    assert!(
        found[0].sentence.ends_with(
            "Auto sizing raises app's cpus while each raise makes it faster or less throttled and the slice \
             has room; runnerctl sizing says when it cannot."
        ),
        "{}",
        found[0].sentence
    );

    let off = cpu_held(&whole(&held), &measured(false));
    assert_eq!(off[0].severity, Severity::Warning);
    assert!(
        off[0]
            .sentence
            .ends_with("Auto sizing is off: set app's cpus to 6, or turn it on."),
        "{}",
        off[0].sentence
    );

    // Three runs at 3 CPUs before a raise to 4, which took as long and were throttled as much.
    let mut runs = throttled_runs(&[Some(300); 7]);
    for run in &mut runs[..3] {
        run.cpu_cap = Some(3000);
    }
    let unhelped = cpu_held(&whole(&runs), &measured(true));
    assert_eq!(
        unhelped[0].severity,
        Severity::Warning,
        "its runs ask for no further raise"
    );
    assert!(
        matches!(
            unhelped[0].kind,
            InsightKind::CpuHeld {
                raised_from: Some(3),
                set: None,
                held: 4,
                ..
            }
        ),
        "{unhelped:#?}"
    );
    assert!(
        unhelped[0].sentence.ends_with(UNHELPED),
        "{}",
        unhelped[0].sentence
    );

    // Less throttled at 4 than at 3: the raise helped, and auto sizing goes on.
    for run in &mut runs[..3] {
        run.cpu_throttled = Some(960);
    }
    let helped = cpu_held(&whole(&runs), &measured(true));
    assert_eq!(helped[0].severity, Severity::Notice, "{helped:#?}");
}

const UNHELPED: &str = "The raise of app from 3 to 4 CPUs did not make it faster or less throttled, as when its tools \
                        start a worker per CPU, so its runs ask for no further raise; runnerctl sizing says what auto \
                        sizing does next.";

/// `runs` as auto sizing reads them, each averaging `average` millicores of CPU.
fn as_sized(runs: &[Run], average: u64) -> Vec<crate::sizing::Ran> {
    runs.iter()
        .map(|run| crate::sizing::Ran {
            repository: run.repository.clone(),
            workflow: run.workflow.clone(),
            name: run.name.clone(),
            profile: run.profile.clone().unwrap(),
            memory_cap: Some(4 * G),
            cpu_cap: run.cpu_cap,
            memory_peak: G,
            cpu_peak: run.cpu_cap.unwrap(),
            cpu_average: Some(average),
            queued_at: run.queued_at,
            started_at: run.started_at,
            runner_started_at: run.runner_started_at.unwrap(),
            finished_at: run.finished_at,
            oom_killed: false,
            hit_cap: None,
            own_cap: None,
            measure: crate::sizing::Measure::WithoutPageCache,
            succeeded: true,
            failed: false,
            run_id: run.run_id,
            run_attempt: 1,
            branch: run.branch.clone(),
            reserved: None,
            host_waited: 0,
            slot_waited: 0,
            overflow_waited: 0,
            unseen: 0,
            cpu_periods: run.cpu_periods,
            cpu_throttled: run.cpu_throttled,
        })
        .collect()
}

/// The `cpus` auto sizing's look at `NOW` raises app to, in a slice of 32 CPUs and 33G, on `jobs`.
fn sizing_raises_app_to(config: &Config, jobs: Vec<crate::sizing::Ran>) -> Option<u32> {
    let history = crate::sizing::History {
        jobs,
        ..crate::sizing::History::default()
    };
    let (last, running) = (BTreeMap::new(), BTreeMap::new());
    let changes = crate::sizing::auto::changes(
        config,
        &history,
        crate::sizing::auto::Context {
            memory_max: Some(33 * G),
            cpus: Some(32_000),
            window: crate::sizing::Window {
                since: NOW - 30 * 24 * HOUR,
                now: NOW,
                sample_interval: 2,
            },
            last: &last,
            cpus_set: &[],
            running: &running,
            disk: None,
        },
    );
    changes
        .iter()
        .filter(|change| change.profile == "app")
        .find_map(|change| change.to.cpus.filter(|to| *to > 4))
}

/// Seven runs of `name` throttled in `before` per mille of their run at 3 CPUs, the first three, and in `now` at 4.
fn raised(name: &str, ids: u64, before: u64, now: u64) -> Vec<Run> {
    let mut runs = throttled_runs(&[
        Some(before),
        Some(before),
        Some(before),
        Some(now),
        Some(now),
        Some(now),
        Some(now),
    ]);
    for run in &mut runs {
        run.name = name.to_owned();
        run.id += ids;
        run.run_id += ids;
    }
    for run in &mut runs[..3] {
        run.cpu_cap = Some(3000);
    }
    runs
}

#[test]
fn a_raise_that_did_not_help_a_job_whose_runs_average_near_the_cap_never_says_auto_sizing_stopped()
{
    // As long and as throttled at 4 CPUs as at 3, but averaging 3 of the 4: auto sizing raises app on the averages.
    let runs = raised("test", 0, 300, 300);
    let config = measured(true);

    assert_eq!(
        sizing_raises_app_to(&config, as_sized(&runs, 3000)),
        Some(6),
        "auto sizing raises app on the runs' averages"
    );
    let found = cpu_held(&whole(&runs), &config);
    assert!(
        found[0].sentence.ends_with(UNHELPED),
        "{}",
        found[0].sentence
    );
    assert!(
        !found[0].sentence.contains("stopped"),
        "it says what this job's runs ask for, not what auto sizing does: {}",
        found[0].sentence
    );
}

#[test]
fn a_raise_that_did_not_help_one_job_but_helped_another_never_says_auto_sizing_stopped() {
    // test is as throttled at 4 CPUs as at 3; lint is still held back at 4, but less than at 3: its raise helped, so
    // auto sizing raises app for lint, light as both jobs' averages are.
    let mut runs = raised("test", 0, 300, 300);
    runs.extend(raised("lint", 100, 400, 200));
    let config = measured(true);

    assert_eq!(
        sizing_raises_app_to(&config, as_sized(&runs, 1000)),
        Some(6),
        "auto sizing raises app for lint"
    );
    let found = cpu_held(&whole(&runs), &config);
    let sentence = |job: &str| {
        found
            .iter()
            .find(|insight| insight.job.as_deref() == Some(job))
            .map(|insight| (insight.severity, insight.sentence.as_str()))
            .unwrap()
    };
    let (_, test) = sentence("test");
    assert!(test.ends_with(UNHELPED), "{test}");
    assert!(!test.contains("stopped"), "{test}");
    let (severity, lint) = sentence("lint");
    assert_eq!(severity, Severity::Notice, "{lint}");
    assert!(lint.contains("Auto sizing raises app's cpus"), "{lint}");
}

/// A job of `halyard/api` that started `hours_ago` hours before `NOW` after waiting `waited` seconds, on a runner
/// started before it was queued when `warm`.
fn waited(n: u64, hours_ago: u64, waited: u64, warm: bool) -> Run {
    let mut run = test_run(n, hours_ago, 60);
    let started = run.started_at.unwrap();
    run.queued_at = started - waited * 1000;
    run.runner_started_at = Some(if warm {
        run.queued_at - 60_000
    } else {
        started - 5_000
    });
    run
}

#[test]
fn waits_are_split_warm_and_cold_and_longer_ones_are_a_notice() {
    let mut runs: Vec<Run> = (0..6).map(|n| waited(n, 200, 20, false)).collect();
    runs.extend((10..16).map(|n| waited(n, 10, 300, false)));
    runs.extend((20..24).map(|n| waited(n, 10, 2, true)));

    let found = insights(&whole(&runs), &config(), 14, NOW);

    let api = &found.waits[0];
    assert_eq!(api.repository, "halyard/api");
    assert_eq!((api.now.warm.jobs, api.now.cold.jobs), (4, 6));
    assert_eq!((api.now.warm.p50, api.now.cold.p50), (Some(2), Some(300)));
    assert_eq!(api.before.all.p95, Some(20));
    let notice = found
        .insights
        .iter()
        .find(|insight| insight.key == "waits:halyard/api")
        .expect("waits went up");
    assert_eq!(notice.severity, Severity::Notice);
    assert_eq!(
        notice.kind,
        InsightKind::Waits {
            before: 20,
            now: 300
        }
    );
}

#[test]
fn a_week_of_fewer_than_20_jobs_tells_its_longest_wait_and_one_of_more_its_1_in_20() {
    let sentence = |now: u64| {
        let mut runs: Vec<Run> = (0..6).map(|n| waited(n, 200, 20, false)).collect();
        runs.extend((10..10 + now).map(|n| waited(n, 10, 300, false)));
        insights(&whole(&runs), &config(), 14, NOW)
            .insights
            .into_iter()
            .find(|insight| insight.key == "waits:halyard/api")
            .expect("waits went up")
            .sentence
    };
    assert_eq!(
        sentence(19),
        "halyard/api: the longest wait for a runner this week is 5m, up from 20s the week before."
    );
    assert_eq!(
        sentence(20),
        "halyard/api: 1 job in 20 waits 5m or more for a runner this week, up from 20s the week before."
    );
}

#[test]
fn a_week_before_the_history_holds_every_job_is_not_compared() {
    // The history starts 8 days ago: the week before holds only the daemon's first hour, six jobs that waited 20
    // seconds. Against that sliver, this week's 5-minute waits would read "up from 20s the week before".
    let mut runs: Vec<Run> = (0..6).map(|n| waited(n, 8 * 24 - 1, 20, false)).collect();
    runs.extend((10..16).map(|n| waited(n, 10, 300, false)));
    let young = History {
        runs,
        kept_since: NOW - 8 * 24 * HOUR,
        ..History::default()
    };
    let found = insights(&young, &config(), 14, NOW);
    let api = &found.waits[0];
    assert_eq!((api.before.all.jobs, api.before.all.p95), (0, None));
    assert_eq!(
        api.now.all.p95,
        Some(300),
        "this week is what the history has"
    );
    assert!(
        found
            .insights
            .iter()
            .all(|insight| !insight.key.starts_with("waits:")),
        "no waits insight: {:?}",
        found.insights
    );
    assert!(
        digest(&young, &config(), NOW)
            .new
            .iter()
            .all(|insight| !insight.key.starts_with("waits:")),
        "nor in the digest"
    );
    let kept = History {
        kept_since: NOW - 14 * 24 * HOUR,
        host: None,
        ..young
    };
    assert!(
        insights(&kept, &config(), 14, NOW)
            .insights
            .iter()
            .any(|insight| insight.key == "waits:halyard/api"),
        "a history that holds the whole week before compares it"
    );
}

#[test]
fn a_job_queued_before_runnerd_first_ran_waits_from_when_it_did() {
    // runnerd first ran 10 hours ago; six jobs had queued three hours before, and its first runners took them a
    // minute after it started. They waited a minute for runnerd, not four hours.
    let first = NOW - 10 * HOUR;
    let runs: Vec<Run> = (0..6)
        .map(|n| {
            let mut run = waited(n, 9, 60, false);
            run.started_at = Some(first + 60_000);
            run.finished_at = Some(first + 120_000);
            run.queued_at = first - 3 * HOUR;
            run
        })
        .collect();
    let history = History {
        runs,
        kept_since: first,
        ..History::default()
    };
    let found = insights(&history, &config(), 14, NOW);
    assert_eq!(found.waits[0].now.all.p95, Some(60));
    let day = digest(&history, &config(), NOW);
    assert_eq!(day.waits[0].waited, 60, "the digest's longest wait too");
}

#[test]
fn the_digest_counts_the_day_and_lists_only_new_insights() {
    // Flaky two days ago, and again today: not new. Slower since today: new.
    let mut runs = vec![attempt(900, 1, "failure"), attempt(900, 2, "success")];
    for run in &mut runs {
        run.finished_at = Some(NOW - 48 * HOUR);
    }
    let mut durations = vec![240; 9];
    durations.extend([400; 5]);
    let mut slow = history(&durations);
    // The last five runs, and so the slow ones, within the day.
    for (run, hours) in slow.iter_mut().rev().zip([1, 3, 5, 7, 9]) {
        run.finished_at = Some(NOW - hours * HOUR);
        run.started_at = Some(NOW - hours * HOUR - 400_000);
    }
    let mut failed = test_run(500, 2, 60);
    failed.run_id = 500;
    failed.conclusion = Some("failure".to_owned());
    slow.push(failed);
    runs.extend(slow);

    let day = digest(&whole(&runs), &config(), NOW);

    assert_eq!((day.finished, day.failed), (6, 1));
    let new: Vec<&str> = day.new.iter().map(|insight| insight.key.as_str()).collect();
    assert_eq!(new, ["slower:halyard/api/CI/test"]);
    assert_eq!(day.waits.len(), 3);
}

#[test]
fn every_kind_of_insight_reads_back_as_the_daemon_sends_it() {
    let mut runs = history(&[
        240, 240, 240, 240, 240, 240, 240, 240, 360, 360, 360, 360, 360, 360,
    ]);
    for run in &mut runs {
        run.memory_peak = Some(37 * G / 10);
    }
    for run in &mut runs[12..] {
        run.sock_throttled = Some(12);
    }
    for run in &mut runs[11..] {
        run.cpu_periods = Some(3600);
        run.cpu_throttled = Some(1800);
    }
    let mut others = vec![attempt(900, 1, "failure"), attempt(900, 2, "success")];
    others.extend((0..6).map(|n| waited(n + 2000, 200, 20, false)));
    others.extend((10..16).map(|n| waited(n + 2000, 10, 300, false)));
    for run in &mut others {
        run.name = "lint".to_owned();
        run.memory_peak = Some(G);
    }
    runs.extend(others);

    let found = insights(&whole(&runs), &config(), 14, NOW);
    let kinds: Vec<&str> = found
        .insights
        .iter()
        .map(|insight| insight.key.split(':').next().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "throttled",
            "cpu-held",
            "near-cap",
            "slower",
            "waits",
            "flaky"
        ],
        "warnings first: {found:#?}"
    );

    let sent = serde_json::to_string(&found).unwrap();
    let read: api::Insights = serde_json::from_str(&sent).unwrap();
    assert_eq!(read, found);
}

#[test]
fn an_insight_of_a_kind_from_a_later_release_still_reads_by_its_sentence() {
    let sent = r#"{"since": 0, "until": 86400, "waits": [], "insights": [{"key": "later:halyard/api",
        "severity": "warning", "repository": "halyard/api", "workflow": null, "job": null,
        "sentence": "halyard/api: something a later release finds.", "kind": "later", "figure": 3, "runs": []}]}"#;

    let read: api::Insights = serde_json::from_str(sent).unwrap();

    assert_eq!(read.insights[0].kind, InsightKind::Unknown);
    assert_eq!(
        read.insights[0].sentence,
        "halyard/api: something a later release finds."
    );
}

/// Two jobs waiting for the host's memory at `NOW`, the longer for `waited` ms, beside a reading that names java and
/// claude.
fn held(waited: u64) -> HostHold {
    HostHold {
        memory: api::HostMemory {
            at: NOW / 1000 - 2,
            total: 64 * G,
            available: 8 * G,
            slice: 18 * G,
            some: None,
            full: None,
            holders: vec![
                api::Holder {
                    name: "java".to_owned(),
                    memory: 9 * G,
                },
                api::Holder {
                    name: "claude".to_owned(),
                    memory: 5 * G,
                },
            ],
            ceiling: None,
            swap: None,
        },
        seen: NOW - 3_000,
        jobs: vec![
            HeldJob {
                repository: "halyard/web".to_owned(),
                workflow: Some("CI".to_owned()),
                name: "lint".to_owned(),
                waited: 5 * MINUTE,
            },
            HeldJob {
                repository: "halyard/api".to_owned(),
                workflow: Some("CI".to_owned()),
                name: "test".to_owned(),
                waited,
            },
        ],
    }
}

/// The insights at `at` of a history with no finished job and `hold` waiting.
fn holding(hold: Option<HostHold>, at: u64) -> Vec<api::Insight> {
    holding_by(hold, &config(), at)
}

/// The same under `config`.
fn holding_by(hold: Option<HostHold>, config: &Config, at: u64) -> Vec<api::Insight> {
    let history = History {
        host: hold,
        ..History::default()
    };
    insights(&history, config, 14, at).insights
}

#[test]
fn a_job_held_half_an_hour_for_the_hosts_memory_is_a_warning_naming_who_holds_it() {
    // Catches a message that blames the wrong repository, drops the holders, or says closing them frees the whole
    // figure the host holds outside CI.
    let found = holding(Some(held(42 * MINUTE)), NOW);
    let [insight] = found.as_slice() else {
        panic!("one insight: {found:#?}");
    };
    assert_eq!(insight.severity, Severity::Warning);
    assert_eq!(insight.repository, "halyard/api", "the longest waiter's");
    assert_eq!(
        insight.kind,
        InsightKind::HostHeld {
            jobs: 2,
            waited: 42 * 60
        }
    );
    assert_eq!(
        insight.sentence,
        "2 jobs wait for the host's memory, the longest for 42m 00s (halyard/api: CI / test); programs outside CI \
         hold 38G, most java 9G and claude 5G. Closing programs frees only part of that."
    );
    // The same for the whole of a 6-hour window and another after it.
    let key = |at: u64| {
        let mut hold = held(42 * MINUTE);
        hold.memory.at = at / 1000;
        hold.seen = at;
        holding(Some(hold), at)[0].key.clone()
    };
    let window = 6 * HOUR;
    let start = NOW / window * window;
    assert_eq!(key(start), key(start + window - 1000));
    assert_ne!(key(start + window - 1000), key(start + window));
}

#[test]
fn nothing_is_said_of_a_host_wait_that_is_short_unseen_stale_or_from_after_the_look() {
    // Catches a message for every short span, one from a daemon that stopped looking, and the digest's look at 24
    // hours ago finding today's wait.
    let said = |hold, at| !holding(hold, at).is_empty();
    assert!(said(Some(held(30 * MINUTE)), NOW), "30 minutes is enough");
    assert!(!said(Some(held(30 * MINUTE - 1)), NOW), "29m 59s is not");
    assert!(!said(None, NOW), "no job waits for it now");
    let mut no_jobs = held(HOUR);
    no_jobs.jobs.clear();
    assert!(!said(Some(no_jobs), NOW));
    let mut old_reading = held(HOUR);
    old_reading.memory.at = NOW / 1000 - 61;
    assert!(!said(Some(old_reading), NOW), "a reading of a minute ago");
    let mut old_look = held(HOUR);
    old_look.seen = NOW - 61_000;
    assert!(!said(Some(old_look), NOW), "the engine's last look");
    // A slow engine plans once a poll: its look is stale only past three of them.
    let slow = Config::parse(&CONFIG.replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\npoll_interval = \"1m\"",
    ))
    .unwrap();
    let mut slow_plan = held(HOUR);
    slow_plan.seen = NOW - 170_000;
    assert!(
        holding_by(Some(slow_plan.clone()), &config(), NOW).is_empty(),
        "170 s is over the default's minute"
    );
    assert!(
        !holding_by(Some(slow_plan.clone()), &slow, NOW).is_empty(),
        "170 s is under three 1-minute polls"
    );
    slow_plan.seen = NOW - 190_000;
    assert!(
        holding_by(Some(slow_plan), &slow, NOW).is_empty(),
        "190 s is over"
    );
    let mut this_second = held(HOUR);
    this_second.seen = NOW + 400;
    assert!(
        said(Some(this_second), NOW),
        "a plan in the second of the look is not from after it"
    );
    let mut next_second = held(HOUR);
    next_second.seen = NOW + 1_000;
    assert!(!said(Some(next_second), NOW), "one in the next is");
    assert!(
        !said(Some(held(HOUR)), NOW - 24 * HOUR),
        "a look from before the reading"
    );
}

#[test]
fn a_host_held_insight_without_names_says_how_much_the_host_holds_and_no_more() {
    // Catches a failure to word a host whose cgroups it cannot read (no holders), or a stray separator.
    let mut hold = held(HOUR);
    hold.memory.holders.clear();
    let found = holding(Some(hold), NOW);
    assert_eq!(
        found[0].sentence,
        "2 jobs wait for the host's memory, the longest for 1h 00m (halyard/api: CI / test); programs outside CI \
         hold 38G. Closing programs frees only part of that."
    );
}

/// Midnight UTC of `NOW`'s day, 8 hours before it.
const TODAY: u64 = NOW / (24 * HOUR) * (24 * HOUR);
const SECOND: u64 = 1000;

/// A reading of a 64G host, whose low line is 20G and critical line 6.4G, `at` ms with `available` and `credit`.
fn reading(at: u64, available: u64, credit: u64, running: bool) -> HostReading {
    HostReading {
        at,
        total: 64 * G,
        available,
        swap_free: Some(8 * G),
        credit,
        running,
    }
}

/// Today: 30s with nothing short, a minute under 20G on a credit, a two-hour gap the daemon did not run, an idle
/// reading at 1G, 20s under 6.4G on a credit, then a minute at 40G. Yesterday: a minute under 6.4G with no credit.
fn readings() -> Vec<HostReading> {
    vec![
        reading(TODAY - 2 * HOUR, 2 * G, 0, true),
        reading(TODAY - 2 * HOUR + 60 * SECOND, 30 * G, 0, false),
        reading(TODAY + HOUR, 30 * G, 0, true),
        reading(TODAY + HOUR + 30 * SECOND, 10 * G, 2 * G, true),
        reading(TODAY + 3 * HOUR, G, 0, false),
        reading(TODAY + 3 * HOUR + 20 * SECOND, 5 * G, 2 * G, true),
        reading(TODAY + 3 * HOUR + 40 * SECOND, 40 * G, 0, true),
    ]
}

#[test]
fn the_host_s_day_counts_only_while_runners_ran_each_reading_for_a_minute_at_most_with_its_share_on_a_credit()
 {
    // Catches a reading counted across a gap the daemon did not run, an idle host's low reading counted as CI's,
    // a day's figures leaking into the next, and a credit's share taken from the wrong readings.
    let history = History {
        readings: readings(),
        ..History::default()
    };
    let found = insights(&history, &config(), 14, NOW);
    let [today, yesterday] = found.host.as_slice() else {
        panic!("two days, newest first: {:#?}", found.host);
    };
    assert_eq!(today.since, TODAY / SECOND);
    assert_eq!((today.running, today.credited), (30 + 60 + 20 + 60, 80));
    assert_eq!((today.low_line, today.critical_line), (20 * G, 64 * G / 10));
    assert_eq!(
        (today.low.seconds, today.low.credited),
        (80, 80),
        "under 20G only on the credit"
    );
    assert_eq!((today.critical.seconds, today.critical.credited), (20, 20));
    assert_eq!(
        (today.lowest, today.lowest_credited),
        (5 * G, Some(5 * G)),
        "the idle 1G is not CI's"
    );
    assert_eq!(
        today.dated_line(),
        "Jan 15: CI ran 2m 50s, 1m 20s of it on a host credit; under 20G available for 1m 20s (1m 20s on a \
         credit); under 6.4G available for 20s (20s on a credit); lowest 5G (5G on a credit); swap free at least 8G"
    );
    assert_eq!((yesterday.running, yesterday.credited), (60, 0));
    assert_eq!(yesterday.critical.seconds, 60);
    assert!(
        !yesterday.line().contains("credit"),
        "a day without a credit says nothing of one: {}",
        yesterday.line()
    );
}

#[test]
fn a_day_under_a_tenth_of_the_host_on_a_credit_warns_once_and_a_day_without_one_does_not() {
    // Catches a warning for a low host the credit had no part in, and a key that changes within the day, which
    // would tell the owner again on every look.
    let history = History {
        readings: readings(),
        ..History::default()
    };
    let warning = |at: u64| {
        let found = insights(&history, &config(), 14, at).insights;
        let [insight] = found.as_slice() else {
            panic!("one warning: {found:#?}");
        };
        insight.clone()
    };
    let insight = warning(NOW);
    assert_eq!(insight.severity, Severity::Warning);
    assert_eq!(
        insight.kind,
        InsightKind::HostLow {
            day: TODAY / SECOND,
            seconds: 20,
            lowest: 5 * G
        }
    );
    assert_eq!(insight.key, warning(NOW + 10 * HOUR).key, "the same day's");
    assert!(
        insight.sentence.contains(
            "[host] pooled_count = false makes the budgets count every reservation again"
        ),
        "{}",
        insight.sentence
    );

    let mut uncredited = readings();
    for reading in &mut uncredited {
        reading.credit = 0;
    }
    let history = History {
        readings: uncredited,
        ..History::default()
    };
    let found = insights(&history, &config(), 14, NOW).insights;
    assert!(
        found.is_empty(),
        "a low host with no credit is no warning: {found:#?}"
    );
}

#[test]
fn the_digest_reads_the_host_over_its_own_24_hours() {
    // Catches a digest that reads the calendar day, or the whole insights window, instead of its 24 hours.
    let history = History {
        readings: readings(),
        ..History::default()
    };
    let day = digest(&history, &config(), NOW + 15 * HOUR).host.unwrap();
    assert_eq!(
        (day.running, day.lowest),
        (170, 5 * G),
        "today's runs, not yesterday's"
    );
    let before = digest(&history, &config(), TODAY - HOUR).host.unwrap();
    assert_eq!((before.running, before.lowest), (60, 2 * G));
    assert_eq!(
        digest(&history, &config(), TODAY + 30 * HOUR).host,
        None,
        "no runner ran in its 24 hours"
    );
}

/// Run `n` of `workflow / name`, finished `hours_ago`, which wrote `bytes`.
fn writing(
    n: u64,
    hours_ago: u64,
    workflow: &str,
    name: &str,
    bytes: u64,
    measure: api::Measure,
) -> Run {
    Run {
        workflow: Some(workflow.to_owned()),
        name: name.to_owned(),
        disk: Some(api::DiskWritten {
            bytes,
            ios: bytes / 4096,
            dirty: Some(bytes / 10),
            measure,
        }),
        ..test_run(n, hours_ago, 60)
    }
}

#[test]
fn disk_writes_rank_workflows_and_jobs_and_set_the_jobs_against_the_drive_they_ended_in() {
    const M: u64 = 1 << 20;
    let complete = api::Measure::Complete;
    let runs = [
        writing(1, 0, "images", "image-api", 3 * G, complete),
        writing(2, 1, "CI", "test", 100 * M, complete),
        writing(3, 2, "CI", "test", 300 * M, api::Measure::Partial),
        writing(4, 30, "CI", "lint", 10 * M, complete),
        // Not measured: no figure, and not counted as 0.
        test_run(5, 1, 60),
    ];
    // The drive was read for the last three hours only, an hour of it before a restart; the last hour's slice was
    // not read.
    let window = |hours_ago: u64, drive: u64, slice: Option<u64>| crate::observe::disk::Window {
        start: NOW - hours_ago * HOUR,
        end: NOW - (hours_ago - 1) * HOUR,
        device: "nvme0n1".to_owned(),
        drive: Some(drive),
        slice,
        daemon: Some(100 * M),
        owners: None,
        commits: Some(7200),
        store: None,
    };
    let history = History {
        disk: vec![
            window(3, 2 * G, Some(G)),
            window(2, 4 * G, Some(2 * G)),
            window(1, 4 * G, None),
        ],
        ..whole(&runs)
    };
    let disk = insights(&history, &config(), 14, NOW).disk.unwrap();
    assert_eq!(
        disk.jobs,
        api::JobWrites {
            measured: 4,
            partial: 1,
            total: 3 * G + 410 * M,
            median: 100 * M,
            p90: 3 * G,
            dirty: 3 * G / 10 + 100 * M / 10 + 300 * M / 10 + 10 * M / 10,
        }
    );
    let workflows: Vec<_> = disk
        .workflows
        .iter()
        .map(|top| (top.workflow.as_deref().unwrap(), top.jobs, top.total))
        .collect();
    assert_eq!(workflows, [("images", 1, 3 * G), ("CI", 3, 410 * M)]);
    assert_eq!(disk.top_jobs[1].job.as_deref(), Some("test"));
    let drive = disk.drive.unwrap();
    assert_eq!(drive.watched, 3 * 3600);
    assert_eq!(drive.written, 10 * G);
    assert_eq!(
        drive.commits,
        Some(api::Commits {
            count: 3 * 7200,
            watched: 3 * 3600
        })
    );
    assert_eq!(
        drive.shares,
        api::DriveShares {
            watched: 2 * 3600,
            written: 6 * G,
            // The image ended where the slice was not read; the lint run while no window read the drive.
            jobs: 400 * M,
            slice: Some(3 * G),
            daemon: Some(200 * M),
            owners: None,
        },
        "one window without the slice leaves the others' shares"
    );
    assert_eq!(drive.per_day(), 80 * G);
}

/// A window of the drive `nvme0n1` (or `device`) `hours_ago` hours before [`NOW`], an hour long, that wrote `drive`
/// bytes, with the runner slice's `slice` and the split `owners`.
fn drive_window(
    device: &str,
    hours_ago: u64,
    drive: u64,
    owners: Option<api::DriveOwners>,
) -> crate::observe::disk::Window {
    crate::observe::disk::Window {
        start: NOW - hours_ago * HOUR,
        end: NOW - (hours_ago - 1) * HOUR,
        device: device.to_owned(),
        drive: Some(drive),
        slice: Some(drive / 4),
        daemon: Some(drive / 100),
        owners,
        commits: None,
        store: None,
    }
}

/// Right after an upgrade the history holds windows from before runnerd split them: the shares are over the split
/// ones only, and say so, so an old window's whole is never set against a split it did not have.
#[test]
fn the_drive_s_shares_are_over_its_split_windows_once_there_are_any() {
    let owners = api::DriveOwners {
        runnerd: Some(G / 10),
        docker: Some(G),
        containerd: Some(0),
        system: G,
        user: 0,
        kernel: 5 * G,
        kernel_ios: 5 * G / 16_384,
    };
    let history = History {
        disk: vec![
            drive_window("nvme0n1", 3, 8 * G, None),
            drive_window("nvme0n1", 2, 8 * G, Some(owners)),
            drive_window("nvme0n1", 1, 8 * G, Some(owners)),
        ],
        ..whole(&[])
    };
    let drive = insights(&history, &config(), 14, NOW)
        .disk
        .unwrap()
        .drive
        .unwrap();
    assert_eq!(
        drive.written,
        24 * G,
        "the drive's figure is over every window"
    );
    assert_eq!(drive.shares.watched, 2 * 3600);
    assert_eq!(drive.shares.written, 16 * G);
    assert_eq!(drive.shares.owners, Some(owners.plus(owners)));
    assert!(
        drive
            .shares
            .line(drive.watched)
            .starts_with("Over the 2h 00m runnerd split the writes by cgroup: "),
        "{}",
        drive.shares.line(drive.watched)
    );
    // Only windows from before: not split, as before.
    let old = History {
        disk: vec![drive_window("nvme0n1", 1, 8 * G, None)],
        ..whole(&[])
    };
    let shares = insights(&old, &config(), 14, NOW)
        .disk
        .unwrap()
        .drive
        .unwrap()
        .shares;
    assert_eq!(shares.owners, None);
    assert!(
        shares
            .line(3600)
            .contains("no owner runnerd knows 74% (not split by cgroup in these windows)"),
        "{}",
        shares.line(3600)
    );
}

const TB: u64 = 1_000_000_000_000;

/// [`CONFIG`] with `[host.drive]` for `nvme0n1` rated 600TB, and `written` its SMART count 24 hours before [`NOW`]
/// when given.
fn drive_config(written: Option<&str>) -> Config {
    let written = written.map_or_else(String::new, |bytes| {
        format!("written = {{ bytes = \"{bytes}\", at = \"2027-01-14T08:00:00Z\" }}\n")
    });
    Config::parse(&CONFIG.replace(
        "admission = \"caps\"\n",
        &format!(
            "admission = \"caps\"\n\n[host.drive]\ndevice = \"nvme0n1\"\nendurance = \"600TB\"\n{written}"
        ),
    ))
    .unwrap()
}

/// The owner's question: how many days until the drive has written its rating. A rate over the time runnerd did not
/// read, the old disk's windows after Docker's root moved, a SMART count read in binary units, or a count without
/// what was written since would each move the date.
#[test]
fn the_drive_s_endurance_counts_from_its_smart_count_at_the_rates_runnerd_read() {
    // Two days ago at 4TB an hour of reading; the last day at 6TB an hour, one hour of it unread; another disk.
    let mut disk = Vec::new();
    for hours_ago in (25..=48).rev() {
        disk.push(drive_window("nvme0n1", hours_ago, TB / 6, None));
    }
    for hours_ago in (1..=24).rev().filter(|hours| *hours != 5) {
        disk.push(drive_window("nvme0n1", hours_ago, TB / 4, None));
    }
    disk.push(drive_window("nvme1n1", 1, 50 * TB, None));
    let history = History {
        disk,
        drive_read: Some(super::DriveRead {
            bytes: 23 * TB / 4,
            watched: 23 * HOUR,
            until: NOW,
        }),
        ..whole(&[])
    };
    let found = insights(&history, &drive_config(Some("77.2TB")), 14, NOW);
    let endurance = found.disk.as_ref().unwrap().endurance.clone().unwrap();
    assert_eq!(endurance.rated, 600 * TB, "decimal, as SMART counts");
    let written = endurance.written.unwrap();
    assert_eq!(written.total(), 77_200_000_000_000 + 23 * TB / 4);
    assert_eq!((written.watched, written.since), (23 * 3600, 24 * 3600));
    assert_eq!(
        endurance.day.unwrap().per_day(),
        6 * TB,
        "over the hours read, not the day"
    );
    let week = 24 * (TB / 6) + 23 * (TB / 4);
    assert_eq!(
        endurance.week.unwrap().per_day(),
        week * 24 / 47,
        "nvme1n1 left out"
    );
    let left = 600 * TB - written.total();
    assert_eq!(
        endurance.days_left(api::RateSpan::Week),
        Some(left / (week * 24 / 47))
    );
    assert_eq!(
        endurance.days_left(api::RateSpan::Day),
        Some(left / (6 * TB))
    );
    assert_eq!(
        endurance.lines(),
        [
            "nvme0n1 is rated to write 600TB (decimal terabytes, as drive makers and SMART count; runnerd's other \
             sizes are binary, 1T = 1.1TB)",
            "It has written at least 83TB: 77.2TB its SMART log said on Jan 14 08:00 UTC, per [host.drive], and \
             5.8TB runnerd read since, over 23h 00m of the 1d 0h since; what it wrote while runnerd did not read it \
             is not counted",
            "At the last 7 days' rate, 5TB a day over the 1d 23h runnerd read it: the 517TB left lasts 103 days",
            "At the last 24 hours' rate, 6TB a day over the 23h 00m runnerd read it: the 517TB left lasts 86 days",
        ]
    );
    // Under the default 180 days: a warning, which attention and notifications carry.
    let warning = found
        .insights
        .iter()
        .find(|insight| matches!(insight.kind, InsightKind::DriveWear { .. }))
        .expect("a drive-wear insight");
    assert_eq!(warning.severity, Severity::Warning);
    assert_eq!(warning.key, "drive-wear:nvme0n1");
    assert_eq!(
        warning.sentence,
        "nvme0n1 reaches its rated 600TB written (decimal terabytes, as SMART counts) in 103 days at the last 7 \
         days' rate of 5TB a day"
    );
    // Without a SMART count: the rates, and no days left or warning.
    let unknown = insights(&history, &drive_config(None), 14, NOW);
    let endurance = unknown.disk.unwrap().endurance.unwrap();
    assert_eq!(endurance.days_left(api::RateSpan::Week), None);
    assert!(!endurance.warns(), "no count, no warning");
    assert!(
        endurance.lines()[1].starts_with("What it has written is unknown"),
        "{:?}",
        endurance.lines()
    );
    // Past its rating: a warning that says so.
    let past = insights(&history, &drive_config(Some("600.5TB")), 14, NOW);
    let warning = past
        .insights
        .iter()
        .find(|insight| matches!(insight.kind, InsightKind::DriveWear { .. }))
        .expect("a drive-wear insight past the rating");
    assert!(
        warning.sentence.contains("has written its rated 600TB"),
        "{}",
        warning.sentence
    );
    // No [host.drive]: nothing.
    let none = insights(&history, &config(), 14, NOW);
    assert_eq!(none.disk.unwrap().endurance, None);
    assert!(
        !none
            .insights
            .iter()
            .any(|insight| matches!(insight.kind, InsightKind::DriveWear { .. })),
        "no rating, no warning"
    );
}

#[test]
fn a_window_without_measured_jobs_or_a_read_drive_has_no_disk_writes() {
    let runs = [test_run(1, 1, 60)];
    assert_eq!(insights(&whole(&runs), &config(), 14, NOW).disk, None);
}

/// A job of profile `profile` queued `waited` seconds before its start, first listed `listed` seconds after it was
/// queued, held for `causes` (seconds by cause), or from before runnerd counted causes when `None`.
fn queued_for(
    n: u64,
    profile: &str,
    waited: u64,
    listed: u64,
    causes: Option<&[(api::WaitCause, u64)]>,
) -> Run {
    let mut run = test_run(n, 1, 60);
    let started = run.started_at.unwrap();
    run.queued_at = started - waited * 1000;
    run.seen_at = Some(run.queued_at + listed * 1000);
    run.profile = Some(profile.to_owned());
    run.causes = causes.map(|causes| {
        causes
            .iter()
            .map(|(cause, seconds)| (*cause, seconds * 1000))
            .collect()
    });
    run
}

// Catches a split that reads a row from before schema 26 as a wait of 0, hides what no cause covers, counts the time
// no runnerd ran twice, or lets causes counted past a start add up to more than the wait.
#[test]
fn a_profiles_queue_time_splits_by_cause_and_names_what_no_cause_covers() {
    use api::WaitCause::{Barrier, Host, Other, Slot};
    let mut down = queued_for(4, "app", 100, 50, Some(&[(Host, 50)]));
    // The daemon was down for the first 40 s: out of the wait, and out of what came before the listing.
    down.unseen = 40_000;
    down.unseen_unlisted = 40_000;
    let runs = [
        queued_for(1, "app", 100, 10, Some(&[(Host, 50), (Barrier, 20)])),
        queued_for(2, "app", 300, 5, None),
        // Counted to the plan after its start: 30 s for a slot and 10 s for a cause a later runnerd stored, in 20 s.
        queued_for(3, "app", 20, 0, Some(&[(Slot, 30), (Other, 10)])),
        down,
        queued_for(5, "light", 60, 0, None),
    ];

    let queue = insights(&whole(&runs), &config(), 14, NOW).queue;

    let app = &queue[0];
    assert_eq!(app.profile, "app");
    assert_eq!((app.jobs, app.unrecorded), (3, 1));
    assert_eq!(app.waited_ms, (100 + 20 + 60) * 1000);
    let causes: Vec<(api::WaitCause, u64)> = app
        .causes
        .iter()
        .map(|cause| (cause.cause, cause.waited_ms / 1000))
        .collect();
    assert_eq!(causes, [(Host, 100), (Barrier, 20), (Slot, 15), (Other, 5)]);
    assert_eq!(
        (app.before_listed_ms, app.uncovered_ms),
        ((10 + 10) * 1000, 20 * 1000)
    );
    let parts: u64 = app.causes.iter().map(|cause| cause.waited_ms).sum::<u64>()
        + app.before_listed_ms
        + app.uncovered_ms;
    assert_eq!(parts, app.waited_ms, "the parts add up to the wait");
    assert_eq!(
        app.line(),
        "app: 3 jobs waited 3m 00s in all; 1 job from before runnerd counted why is left out; 1 job's causes are \
         estimates, scaled into its wait"
    );
    let light = &queue[1];
    assert_eq!((light.jobs, light.unrecorded), (0, 1));
    assert!(light.figures().is_empty(), "no job of light was counted");
    assert_eq!(
        light.line(),
        "light: 1 job ended from before runnerd counted why jobs wait"
    );
}

/// The figure `label` of `profile`'s split, if it shows.
fn queue_figure(profile: &api::ProfileQueue, label: &str) -> Option<String> {
    profile
        .figures()
        .into_iter()
        .find(|figure| figure.label == label)
        .map(|figure| figure.value)
}

// Catches causes counted past a job's start taking the time before runnerd listed it: that time is known on its own,
// so it is kept whole, and the causes are scaled into what is left and said to be estimates.
#[test]
fn causes_counted_past_a_start_leave_the_time_before_the_listing_whole() {
    use api::WaitCause::{Booting, Host};
    // Queued at 0 s, listed at 45 s, held for the host from 50 s to 60 s, started at 70 s, and its take read at
    // 100 s: 40 s of booting counted.
    let runs = [queued_for(
        1,
        "app",
        70,
        45,
        Some(&[(Host, 10), (Booting, 40)]),
    )];

    let queue = insights(&whole(&runs), &config(), 14, NOW).queue;

    let app = &queue[0];
    assert_eq!(
        queue_figure(app, api::ProfileQueue::BEFORE_LISTED).as_deref(),
        Some("45s")
    );
    assert_eq!(queue_figure(app, Host.label()).as_deref(), Some("5s"));
    assert_eq!(queue_figure(app, Booting.label()).as_deref(), Some("20s"));
    assert_eq!(queue_figure(app, api::ProfileQueue::UNCOVERED), None);
    assert!(
        app.line().contains("1 job's causes are estimates"),
        "{}",
        app.line()
    );
}

// Catches a restart while a listed job still queues taking its downtime off the time before the listing, which it
// came after.
#[test]
fn downtime_after_the_listing_is_not_taken_off_the_time_before_it() {
    use api::WaitCause::Host;
    // Queued at 0 s, listed at 30 s, no runnerd from 60 s to 100 s, started at 130 s; plans held it 40 s.
    let mut run = queued_for(1, "app", 130, 30, Some(&[(Host, 40)]));
    run.unseen = 40_000;

    let queue = insights(&whole(&[run]), &config(), 14, NOW).queue;

    let app = &queue[0];
    assert_eq!(
        queue_figure(app, api::ProfileQueue::BEFORE_LISTED).as_deref(),
        Some("30s")
    );
    assert_eq!(
        queue_figure(app, api::ProfileQueue::UNCOVERED).as_deref(),
        Some("20s")
    );
}

// Catches shares taken from rounded seconds: two causes of 600 ms in a wait of 1.2 s each read 100%.
#[test]
fn shares_of_a_wait_under_a_second_come_from_its_milliseconds() {
    use api::WaitCause::{Host, Slot};
    let mut run = queued_for(1, "app", 0, 0, None);
    run.queued_at -= 1200;
    run.seen_at = Some(run.queued_at);
    run.causes = Some(BTreeMap::from([(Host, 600), (Slot, 600)]));

    let queue = insights(&whole(&[run]), &config(), 14, NOW).queue;

    let shares: Vec<Option<String>> = queue[0]
        .figures()
        .into_iter()
        .map(|figure| figure.of)
        .collect();
    assert_eq!(shares, [Some("50%".to_owned()), Some("50%".to_owned())]);
}

/// `count` runs of `app`, newest last, one an hour, whose store held `held` against a 3 GiB share; `None` for an image
/// that reported nothing.
fn stored(count: u64, held: impl Fn(u64) -> Option<u64>) -> Vec<Run> {
    (0..count)
        .map(|n| Run {
            docker_store: held(n)
                .map(|held| crate::cache::shares::StoreReport { held, share_gib: 3 }),
            ..test_run(n, 1 + count - n, 60)
        })
        .collect()
}

fn store_insights(runs: &[Run]) -> Vec<api::Insight> {
    insights(&whole(runs), &config(), api::INSIGHT_DAYS, NOW)
        .insights
        .into_iter()
        .filter(|insight| matches!(insight.kind, InsightKind::StoreOverflow { .. }))
        .collect()
}

// Catches: an insight that never fires on a profile thrashing its store, or one that speaks for a share long since
// raised, from overflows before the last 20 reports.
#[test]
fn a_profile_whose_store_outgrew_its_share_after_most_recent_jobs_is_told() {
    // 18 of the last 20 held 4 GiB of a 3 GiB share.
    let runs = stored(20, |n| Some(if n % 10 == 0 { 2 * G } else { 4 * G }));
    let found = store_insights(&runs);
    assert_eq!(found.len(), 1, "{found:?}");
    let insight = &found[0];
    assert_eq!(insight.severity, Severity::Notice);
    assert_eq!(
        insight.kind,
        InsightKind::StoreOverflow {
            profile: "app".to_owned(),
            overflowed: 18,
            total: 20,
            held: 4 * G,
            share: 3 * G,
        }
    );
    assert_eq!(
        insight.sentence,
        "app: the slots' Docker stores held more than their share after 18 of the last 20 jobs, 4G (p90) against \
         3G, so each such job's images and build layers went and the next one pulled and built them again; more \
         free space on Docker's disk raises every share."
    );
    assert_eq!(insight.runs.len(), 5, "the newest overflowing runs link it");
    assert_eq!(insight.runs[0].job, runs[19].id, "the newest first");

    // Once the share follows them, the stores fit: older overflows do not keep it alive.
    let mut fixed = stored(40, |n| Some(if n < 20 { 4 * G } else { 2 * G }));
    for run in &mut fixed[20..] {
        run.docker_store = Some(crate::cache::shares::StoreReport {
            held: 4 * G,
            share_gib: 5,
        });
    }
    assert!(
        store_insights(&fixed).is_empty(),
        "the last 20 fit their 5G"
    );
}

// Catches: an insight from a handful of jobs, or from images before the report, read as fitting or overflowing.
#[test]
fn few_reports_or_a_store_that_fits_most_of_the_time_is_no_insight() {
    assert!(
        store_insights(&stored(9, |_| Some(4 * G))).is_empty(),
        "9 reports"
    );
    assert_eq!(
        store_insights(&stored(30, |n| (n % 3 == 0).then_some(4 * G))).len(),
        1,
        "10 reports among 30 runs, all over, make one"
    );
    assert_eq!(
        store_insights(&stored(20, |n| Some(if n % 2 == 0 { 4 * G } else { G }))).len(),
        1,
        "half of them is enough"
    );
    assert!(
        store_insights(&stored(20, |n| Some(if n % 3 == 0 { 4 * G } else { G }))).is_empty(),
        "7 of 20 is not"
    );
}

/// After an upgrade from 0.1.5 the old windows read the slice and the new ones are split too: the slice's figure
/// over all of them filled the split stretch's whole and left every cgroup's share at 0%.
#[test]
fn the_slice_s_share_is_over_the_same_split_windows_as_the_others() {
    let owners = api::DriveOwners {
        system: G,
        kernel: 5 * G,
        ..api::DriveOwners::default()
    };
    let history = History {
        disk: vec![
            drive_window("nvme0n1", 3, 8 * G, None),
            drive_window("nvme0n1", 2, 8 * G, Some(owners)),
            drive_window("nvme0n1", 1, 8 * G, Some(owners)),
        ],
        ..whole(&[])
    };
    let shares = insights(&history, &config(), 14, NOW)
        .disk
        .unwrap()
        .drive
        .unwrap()
        .shares;
    assert_eq!(
        shares.slice,
        Some(4 * G),
        "the slice's of the split windows only"
    );
    let kernel = shares
        .parts()
        .into_iter()
        .find(|part| part.owner == api::ShareOwner::Kernel)
        .map(|part| part.bytes);
    assert_eq!(kernel, Some(10 * G));
}

/// The rating's section and its warning show together: a quiet day with no jobs and no windows dropped the section
/// while the warning stood, and a drive past its rating claimed 0 days left beside "has written its rating".
#[test]
fn a_drive_past_its_rating_shows_its_endurance_on_a_quiet_day_and_claims_no_days() {
    let mut disk = Vec::new();
    for hours_ago in 30..=40 {
        disk.push(drive_window("nvme0n1", hours_ago, TB / 4, None));
    }
    let history = History {
        disk,
        drive_read: Some(super::DriveRead::default()),
        ..whole(&[])
    };
    let found = insights(&history, &drive_config(Some("601TB")), 1, NOW);
    let disk = found
        .disk
        .expect("the endurance shows without the day's writes");
    assert_eq!(disk.drive, None);
    let endurance = disk.endurance.unwrap();
    assert_eq!(endurance.left(), Some(0));
    assert!(endurance.week.is_some(), "a rate from before the day");
    assert_eq!(endurance.days_left(api::RateSpan::Week), None);
    let wear = found
        .insights
        .iter()
        .find(|insight| insight.key == "drive-wear:nvme0n1")
        .unwrap();
    assert!(
        matches!(
            wear.kind,
            InsightKind::DriveWear {
                days: None,
                left: Some(0),
                ..
            }
        ),
        "past its rating: no days"
    );
    assert_eq!(
        wear.figure().map(|figure| figure.label),
        Some("rated writes, written".to_owned())
    );
}

/// The digest looks 24 hours back to tell which insights are new. Reading today's writes and a count given today at
/// that earlier time said the drive already warned then, so a drive that crossed its rating today was never new.
#[test]
fn a_drive_that_crossed_its_rating_today_is_new_in_the_digest() {
    let wear = |history: &History, config: &Config| {
        digest(history, config, NOW)
            .new
            .iter()
            .any(|insight| insight.key == "drive-wear:nvme0n1")
    };
    // Counted 599TB two days ago; 2TB written in the last 24 hours, nothing read before.
    let mut config = drive_config(Some("599TB"));
    config
        .host
        .drive
        .as_mut()
        .unwrap()
        .written
        .as_mut()
        .unwrap()
        .at = (NOW - 48 * HOUR) / 1000;
    let history = History {
        disk: vec![
            drive_window("nvme0n1", 3, TB, None),
            drive_window("nvme0n1", 2, TB, None),
        ],
        drive_read: Some(super::DriveRead {
            bytes: 2 * TB,
            watched: 2 * HOUR,
            until: NOW,
        }),
        ..whole(&[])
    };
    assert!(wear(&history, &config), "crossed today: new");
    // Counted past the rating two hours ago, the first count runnerd has.
    let mut config = drive_config(Some("601TB"));
    config
        .host
        .drive
        .as_mut()
        .unwrap()
        .written
        .as_mut()
        .unwrap()
        .at = (NOW - 2 * HOUR) / 1000;
    let history = History {
        drive_read: Some(super::DriveRead::default()),
        ..whole(&[])
    };
    assert!(wear(&history, &config), "given today: new");
}

/// The daemon asks at whole seconds while a window closes at a millisecond: a window that closed in this second is in
/// the history but not in what the store read since the count, and taking it off that read again undercounted it.
#[test]
fn a_window_closed_in_this_second_is_not_taken_off_a_read_that_never_had_it() {
    let mut closing = drive_window("nvme0n1", 1, TB, None);
    closing.end += 350;
    let history = History {
        disk: vec![
            drive_window("nvme0n1", 3, TB, None),
            drive_window("nvme0n1", 2, TB, None),
            closing,
        ],
        drive_read: Some(super::DriveRead {
            bytes: 2 * TB,
            watched: 2 * HOUR,
            until: NOW,
        }),
        ..whole(&[])
    };
    let written = insights(&history, &drive_config(Some("100TB")), 14, NOW)
        .disk
        .unwrap()
        .endurance
        .unwrap()
        .written
        .unwrap();
    assert_eq!((written.read, written.watched), (2 * TB, 2 * 3600));
}
