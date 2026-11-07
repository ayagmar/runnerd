//! When a repository is active and how many jobs its runs are expected to queue still (ADR 0050, 0107).

use super::{Attempt, Learned, Pool, Runs, Seen, target};

const MINUTE: u64 = 60 * 1000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// Some day at midnight UTC, in milliseconds.
const MIDNIGHT: u64 = 20_000 * DAY;

/// The first attempt of run `run_id`.
fn first(run_id: u64) -> Attempt {
    Attempt {
        run_id,
        run_attempt: 1,
    }
}

/// `repository`'s pool in profile `app`.
fn pool(repository: &str) -> Pool {
    Pool {
        repository: repository.to_owned(),
        profile: "app".to_owned(),
    }
}

/// What `history` teaches by `now`, each job for the `light` pool when it asks for `light`, else for `app`.
fn learn(history: &[Seen], now: u64) -> Learned {
    Learned::new(history, now, |seen| {
        Some(if seen.labels.iter().any(|label| label == "light") {
            "light".to_owned()
        } else {
            "app".to_owned()
        })
    })
}

fn seen(repository: &str, workflow: &str, run_id: u64, queued_at: u64) -> Seen {
    Seen {
        repository: repository.to_owned(),
        labels: vec!["self-hosted".to_owned()],
        workflow: Some(workflow.to_owned()),
        run_id,
        run_attempt: 1,
        queued_at,
        finished_at: None,
    }
}

#[test]
fn a_repository_stays_active_ten_minutes_after_its_last_job_whatever_the_hour() {
    // Catches a window left at an hour, and the usual-hours rule left in: o/app's jobs came at 9 UTC on three days,
    // which used to keep it active at 9 every day, a warm runner that took no job.
    let today = MIDNIGHT + 10 * DAY;
    let mut history: Vec<Seen> = (1..=3)
        .map(|day| {
            seen(
                "o/app",
                "CI",
                day,
                today - day * DAY + 9 * HOUR + 20 * MINUTE,
            )
        })
        .collect();
    let now = today + 9 * HOUR + 5 * MINUTE;
    history.push(seen("o/app", "CI", 4, now - 9 * MINUTE));
    let learned = learn(&history, now);
    assert!(
        learned.active("o/app", now),
        "a job nine minutes ago keeps it active"
    );
    assert!(
        !learned.active("o/app", now + 2 * MINUTE),
        "eleven minutes after its last job it is quiet, in the hour its jobs usually come"
    );
    assert!(
        !learned.active("o/other", now),
        "a repository without jobs is quiet"
    );
}

#[test]
fn a_job_seen_now_makes_a_quiet_repository_active() {
    let now = MIDNIGHT + 15 * HOUR;
    let mut learned = learn(&[], now);
    learned.note("o/app", now);
    assert!(
        learned.active("o/app", now + 9 * MINUTE),
        "its new job keeps it active"
    );
}

#[test]
fn a_job_that_finished_keeps_its_repository_active_past_its_end_after_a_restart() {
    // Catches a restart forgetting the finish: a 40-minute job that ended 5 minutes ago would leave the owner's next
    // push cold.
    let now = MIDNIGHT + 15 * HOUR;
    let long = Seen {
        finished_at: Some(now - 5 * MINUTE),
        ..seen("o/app", "CI", 1, now - 45 * MINUTE)
    };
    assert!(
        learn(&[long], now).active("o/app", now),
        "its end 5 minutes ago keeps it active"
    );
}

/// Run `run` of `o/app`'s `CI` at `at`: `heavy` jobs for the `app` pool, and one `light` job, as every run has.
fn ci_run(run: u64, at: u64, heavy: usize) -> Vec<Seen> {
    let mut jobs = vec![Seen {
        labels: vec!["light".to_owned()],
        ..seen("o/app", "CI", run, at)
    }];
    jobs.extend((0..heavy).map(|_| seen("o/app", "CI", run, at + MINUTE)));
    jobs
}

#[test]
fn a_pool_more_than_half_a_workflows_runs_skip_expects_nothing_of_its_next_run() {
    // Catches the median over the runs that used the pool alone: a `ci` run that queues only its `gate` job asked
    // for 4 jvm-small runners, as the runs that used jvm-small did.
    let now = MIDNIGHT + 12 * HOUR;
    let runs = |used: u64, skipped: u64| -> Learned {
        let history: Vec<Seen> = (1..=used + skipped)
            .flat_map(|run| ci_run(run, now - run * HOUR, if run <= used { 4 } else { 0 }))
            .collect();
        learn(&history, now)
    };
    let expected = |learned: &Learned| {
        let mut live = Runs::default();
        let light = Pool {
            profile: "light".to_owned(),
            ..pool("o/app")
        };
        live.note(&light, first(100), 1000, Some("CI"), now);
        live.remaining(&pool("o/app"), learned, now)
    };
    assert_eq!(expected(&runs(3, 4)), 0, "3 runs of 7 used it");
    assert_eq!(expected(&runs(4, 4)), 0, "half of them");
    assert_eq!(expected(&runs(5, 4)), 4, "5 runs of 9 used it, 4 jobs each");
}

#[test]
fn a_workflow_with_too_few_runs_for_the_pool_expects_nothing() {
    // Catches `MIN_RUNS` counting every run: two runs with 3 jobs and one with none have a median of 3, but two
    // runs say little of the next.
    let now = MIDNIGHT + 12 * HOUR;
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| ci_run(run, now - run * HOUR, if run <= 2 { 3 } else { 0 }))
        .collect();
    let learned = learn(&history, now);
    let mut runs = Runs::default();
    runs.note(&pool("o/app"), first(10), 100, Some("CI"), now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        0,
        "two runs with jobs for app say nothing yet"
    );
}

#[test]
fn jobs_with_no_workflow_or_no_pool_count_for_activity_and_for_no_burst() {
    // Catches unknown inputs reaching the burst: a job no pool serves, or of no known workflow, counted as a run of
    // none for the pool, or as one with jobs for it.
    let now = MIDNIGHT + 12 * HOUR;
    let mut history: Vec<Seen> = (1..=3)
        .flat_map(|run| ci_run(run, now - run * HOUR, 2))
        .collect();
    // Five runs no pool serves, and five of no known workflow, each of one job: as runs of none for `app`, they
    // would make its median 0.
    history.extend((10..15).map(|run| Seen {
        labels: vec!["gpu".to_owned()],
        ..seen("o/app", "CI", run, now - 30 * MINUTE)
    }));
    history.extend((20..25).map(|run| Seen {
        workflow: None,
        ..seen("o/app", "CI", run, now - 5 * MINUTE)
    }));
    let learned = Learned::new(&history, now, |seen| {
        if seen.labels.iter().any(|label| label == "gpu") {
            None
        } else if seen.labels.iter().any(|label| label == "light") {
            Some("light".to_owned())
        } else {
            Some("app".to_owned())
        }
    });
    let mut runs = Runs::default();
    runs.note(&pool("o/app"), first(100), 1000, Some("CI"), now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        1,
        "2 per run"
    );
    assert!(
        learned.active("o/app", now),
        "the job of no known workflow 5 minutes ago keeps it active"
    );
}

#[test]
fn a_run_seen_starting_expects_the_rest_of_its_workflows_jobs() {
    let now = MIDNIGHT + 12 * HOUR;
    // Three runs of three jobs each, the last queued five minutes after the first.
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| {
            let first = now - run * HOUR;
            [
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first + 5 * MINUTE),
            ]
        })
        .collect();
    let learned = learn(&history, now);
    let mut runs = Runs::default();
    runs.note(&pool("o/app"), first(10), 100, Some("CI"), now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        2,
        "two more jobs of the run to come"
    );
    runs.note(&pool("o/app"), first(10), 101, None, now + MINUTE);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now + MINUTE),
        1,
        "one more to come"
    );
    assert_eq!(
        runs.remaining(&pool("o/other"), &learned, now),
        0,
        "other repositories expect none"
    );
    // Its span (5 minutes) and IDLE_TIMEOUT (5 minutes) later, it no longer counts.
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now + 11 * MINUTE),
        0,
        "the run stopped counting"
    );
}

#[test]
fn a_run_whose_workflow_is_not_known_expects_nothing_until_it_is() {
    let now = MIDNIGHT + 12 * HOUR;
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| (0..2).map(move |_| seen("o/app", "CI", run, now - run * HOUR)))
        .collect();
    let learned = learn(&history, now);
    let mut runs = Runs::default();
    runs.note(&pool("o/app"), first(10), 100, None, now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        0,
        "its workflow is not known"
    );
    runs.note(&pool("o/app"), first(10), 100, Some("CI"), now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        1,
        "a CI run has two jobs"
    );
}

#[test]
fn warm_zero_turns_off_warm_runners_and_bursts() {
    assert_eq!(target(0, true, 3), 0, "warm = 0 wants none");
    assert_eq!(target(1, false, 0), 0, "a quiet repository wants none");
    assert_eq!(target(1, true, 2), 3, "one warm plus two of a run");
    assert_eq!(target(1, false, 2), 2, "a run's jobs while quiet");
}

#[test]
fn a_run_counts_from_its_first_job_however_late_it_is_seen_again() {
    let now = MIDNIGHT + 12 * HOUR;
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| {
            let first = now - run * HOUR;
            [
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first),
            ]
        })
        .collect();
    let learned = learn(&history, now - 2 * MINUTE);
    let mut runs = Runs::default();
    // After a restart: the history's job, queued 30 minutes ago, then its runner's capture now.
    runs.note(
        &pool("o/app"),
        first(10),
        100,
        Some("CI"),
        now - 30 * MINUTE,
    );
    runs.note(&pool("o/app"), first(10), 100, None, now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        0,
        "the run stopped counting long ago"
    );
}

#[test]
fn a_re_run_of_a_run_that_stopped_counting_expects_its_jobs_again() {
    let now = MIDNIGHT + 12 * HOUR;
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| {
            let first = now - run * HOUR;
            [
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first),
            ]
        })
        .collect();
    let learned = learn(&history, now);
    let mut runs = Runs::default();
    runs.note(&pool("o/app"), first(10), 100, Some("CI"), now - 2 * HOUR);
    // "Re-run failed jobs": the same run, its second attempt.
    let again = Attempt {
        run_id: 10,
        run_attempt: 2,
    };
    runs.note(&pool("o/app"), again, 200, Some("CI"), now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        2,
        "the re-run's other jobs are expected"
    );
    assert_eq!(runs.latest(10), again);
}

#[test]
fn a_re_runs_first_job_taken_before_its_attempt_was_known_counts_for_the_re_run() {
    let now = MIDNIGHT + 12 * HOUR;
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| {
            let first = now - run * HOUR;
            [
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first),
                seen("o/app", "CI", run, first),
            ]
        })
        .collect();
    let learned = learn(&history, now);
    let mut runs = Runs::default();
    runs.note(&pool("o/app"), first(10), 100, Some("CI"), now - 2 * HOUR);
    // A warm runner takes the re-run's first job: its capture names only the run, so it goes to the attempt known.
    runs.note(&pool("o/app"), runs.latest(10), 200, None, now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        0,
        "booked to attempt 1"
    );
    // GitHub's details of the job say attempt 2.
    let again = Attempt {
        run_id: 10,
        run_attempt: 2,
    };
    runs.place(&pool("o/app"), again, 200, Some("CI"), now);
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        2,
        "the re-run's other jobs are expected"
    );
    assert_eq!(runs.latest(10), again);
}

#[test]
fn a_runs_jobs_are_expected_of_the_pool_whose_labels_they_ask_for() {
    let now = MIDNIGHT + 12 * HOUR;
    // Three runs of one heavy job and two light ones.
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| {
            let first = now - run * HOUR;
            let light = |at| Seen {
                labels: vec!["light".to_owned()],
                ..seen("o/app", "CI", run, at)
            };
            [
                seen("o/app", "CI", run, first),
                light(first),
                light(first + MINUTE),
            ]
        })
        .collect();
    let learned = learn(&history, now);
    let light = Pool {
        profile: "light".to_owned(),
        ..pool("o/app")
    };
    let mut runs = Runs::default();
    runs.note(&light, first(10), 100, Some("CI"), now);
    assert_eq!(
        runs.remaining(&light, &learned, now),
        1,
        "one more light job of the run to come"
    );
    assert_eq!(
        runs.remaining(&pool("o/app"), &learned, now),
        1,
        "the heavy pool still expects its own job, whatever the light pool saw"
    );
}

#[test]
fn a_pool_whose_jobs_wait_for_another_pools_still_expects_them_until_they_come() {
    let now = MIDNIGHT + 12 * HOUR;
    // Three runs: a heavy job, then 20 minutes later two light jobs that need it.
    let history: Vec<Seen> = (1..=3)
        .flat_map(|run| {
            let first = now - run * HOUR;
            let light = |at| Seen {
                labels: vec!["light".to_owned()],
                ..seen("o/app", "CI", run, at)
            };
            [
                seen("o/app", "CI", run, first),
                light(first + 20 * MINUTE),
                light(first + 21 * MINUTE),
            ]
        })
        .collect();
    let learned = learn(&history, now);
    let light = Pool {
        profile: "light".to_owned(),
        ..pool("o/app")
    };
    let mut runs = Runs::default();
    runs.note(&pool("o/app"), first(10), 100, Some("CI"), now);
    assert_eq!(
        runs.remaining(&light, &learned, now + 10 * MINUTE),
        2,
        "ten minutes in, the light jobs are still to come"
    );
    // The light span (21 minutes) and IDLE_TIMEOUT (5 minutes) later, it no longer counts.
    assert_eq!(runs.remaining(&light, &learned, now + 27 * MINUTE), 0);
}
