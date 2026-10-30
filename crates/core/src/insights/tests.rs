//! Insights from synthetic histories of fictional repositories: one `halyard/api` job run a few times a day.

use super::{History, Run, digest, insights};
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
    }
}

/// `runs` in a history that holds every job however old.
fn whole(runs: &[Run]) -> History {
    History {
        runs: runs.to_vec(),
        kept_since: 0,
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
        ["throttled", "near-cap", "slower", "waits", "flaky"],
        "warnings first: {found:#?}"
    );

    let sent = serde_json::to_string(&found).unwrap();
    let read: api::Insights = serde_json::from_str(&sent).unwrap();
    assert_eq!(read, found);
}
