use super::*;
use crate::api::{JobMetrics, LogSummary};

fn job(state: JobState, conclusion: Option<&str>) -> JobSummary {
    JobSummary {
        id: 7,
        repository: "halyard/ledger".to_owned(),
        run_id: 1,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: "build".to_owned(),
        branch: None,
        url: None,
        labels: vec![],
        state,
        conclusion: conclusion.map(str::to_owned),
        runner: None,
        timeline: Timeline {
            queued_at: 100,
            ..Timeline::default()
        },
        metrics: JobMetrics::default(),
        log: LogSummary::default(),
    }
}

fn timed(state: JobState, timeline: Timeline) -> JobSummary {
    JobSummary {
        timeline,
        ..job(state, None)
    }
}

fn step(status: &str, conclusion: Option<&str>) -> Step {
    Step {
        number: 1,
        name: "test".to_owned(),
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        started_at: Some(100),
        completed_at: None,
    }
}

fn sample(at: u64, memory: u64, cpu: u64) -> Sample {
    Sample {
        at,
        cpu,
        memory,
        network_received: 0,
        network_sent: 0,
    }
}

#[test]
fn a_job_another_machine_ran_is_neither_its_success_nor_its_failure() {
    for conclusion in ["success", "failure", "cancelled"] {
        let elsewhere = JobSummary {
            conclusion: Some(conclusion.to_owned()),
            ..timed(
                JobState::Elsewhere,
                Timeline {
                    queued_at: 100,
                    started_at: Some(110),
                    finished_at: Some(130),
                    ..Timeline::default()
                },
            )
        };
        assert_eq!(
            elsewhere.outcome(),
            Outcome::new("elsewhere", OutcomeKind::Ended),
            "{conclusion}"
        );
        assert!(
            !elsewhere.is_failure(),
            "another machine's {conclusion} job is not counted"
        );
    }
}

#[test]
fn a_job_cancelled_in_the_queue_did_not_run_elsewhere() {
    let cancelled = job(JobState::Elsewhere, Some("cancelled"));
    assert_eq!(
        cancelled.outcome(),
        Outcome::new("cancelled in queue", OutcomeKind::Ended)
    );
    assert_eq!(
        job(JobState::Elsewhere, Some("skipped")).outcome(),
        Outcome::new("skipped", OutcomeKind::Ended)
    );
    assert!(!cancelled.is_failure(), "a cancelled job did not fail");
    assert_eq!(cancelled.waited(160), None);
}

#[test]
fn a_timed_out_job_failed_and_reads_timed_out() {
    let timed_out = job(JobState::Completed, Some("timed_out"));
    assert_eq!(
        timed_out.outcome(),
        Outcome::new("timed out", OutcomeKind::Failed)
    );
    assert!(timed_out.is_failure(), "a timed-out job counts as failed");
}

#[test]
fn a_job_reads_as_failed_exactly_when_it_counts_as_failed() {
    let conclusions = [
        None,
        Some("success"),
        Some("failure"),
        Some("timed_out"),
        Some("cancelled"),
        Some("skipped"),
        Some("startup_failure"),
        Some("action_required"),
    ];
    let states = [
        JobState::Queued,
        JobState::Running,
        JobState::Completed,
        JobState::Lost,
        JobState::Elsewhere,
    ];
    for state in states {
        for conclusion in conclusions {
            let job = job(state, conclusion);
            assert_eq!(
                job.outcome().kind == OutcomeKind::Failed,
                job.is_failure(),
                "{state:?} {conclusion:?}"
            );
        }
    }
}

#[test]
fn a_waiting_job_has_waited_until_now_and_one_that_ended_unstarted_has_no_wait() {
    assert_eq!(job(JobState::Queued, None).waited(160), Some(60));
    assert_eq!(job(JobState::Elsewhere, None).waited(160), None);
}

#[test]
fn a_final_job_without_a_finish_time_claims_no_run_time() {
    let started = Timeline {
        queued_at: 100,
        started_at: Some(110),
        ..Timeline::default()
    };
    assert_eq!(timed(JobState::Running, started.clone()).ran(170), Some(60));
    assert_eq!(timed(JobState::Elsewhere, started).ran(170), None);
}

#[test]
fn a_runner_starting_shows_before_the_job_starts() {
    let starting = timed(
        JobState::Queued,
        Timeline {
            queued_at: 100,
            container_started_at: Some(120),
            ..Timeline::default()
        },
    );
    let phases: Vec<(PhaseKind, u64, bool)> = starting
        .phases()
        .iter()
        .map(|phase| (phase.kind, phase.seconds(150), phase.to.is_none()))
        .collect();
    assert_eq!(
        phases,
        [
            (PhaseKind::Waiting, 20, false),
            (PhaseKind::Starting, 30, true),
        ]
    );
}

#[test]
fn a_job_that_ended_without_a_runner_stopped_waiting_when_it_ended() {
    let cancelled = timed(
        JobState::Elsewhere,
        Timeline {
            queued_at: 100,
            finished_at: Some(130),
            ..Timeline::default()
        },
    );
    let phases = cancelled.phases();
    assert_eq!(phases.len(), 1, "only the wait: {phases:?}");
    assert_eq!(
        (phases[0].kind, phases[0].seconds(1000)),
        (PhaseKind::Waiting, 30)
    );
}

#[test]
fn a_warm_runners_container_start_is_the_first_milestone() {
    let warm = Timeline {
        queued_at: 100,
        container_started_at: Some(40),
        started_at: Some(101),
        ..Timeline::default()
    };
    let labels: Vec<&str> = warm.milestones().iter().map(|m| m.label).collect();
    assert_eq!(labels, ["Container started", "Queued", "Job started"]);
}

#[test]
fn a_timed_out_step_failed_and_a_cancelled_one_did_not() {
    assert_eq!(
        step("completed", Some("timed_out")).outcome().kind,
        OutcomeKind::Failed
    );
    assert_eq!(
        step("completed", Some("cancelled")).outcome().kind,
        OutcomeKind::Ended
    );
    assert_eq!(
        step("in_progress", None).outcome().kind,
        OutcomeKind::Running
    );
    assert_eq!(step("in_progress", None).took(130), Some(30));
}

#[test]
fn a_running_jobs_peak_rises_with_its_samples_and_survives_a_lagging_read() {
    let mut running = Job {
        summary: job(JobState::Running, None),
        steps: vec![],
        record: None,
        admission: None,
    };
    running.summary.metrics.memory_peak = Some(2 << 30);
    let stored = running.clone();
    running.observe(&sample(200, 3 << 30, 500));
    assert_eq!(running.summary.metrics.memory_peak, Some(3 << 30));
    running.reread(stored.clone());
    assert_eq!(
        running.summary.metrics.memory_peak,
        Some(3 << 30),
        "a read that lags the samples keeps the high-water mark"
    );
    let mut finished = stored;
    finished.summary.state = JobState::Completed;
    running.reread(finished);
    assert_eq!(
        running.summary.metrics.memory_peak,
        Some(2 << 30),
        "a final job's peak is the daemon's total"
    );
}

#[test]
fn a_run_that_failed_since_its_runs_were_read_counts_as_failed() {
    let mut earlier = job(JobState::Completed, Some("success"));
    earlier.id = 6;
    earlier.timeline.started_at = Some(110);
    earlier.timeline.finished_at = Some(170);
    let mut this = Job {
        summary: job(JobState::Running, None),
        steps: vec![],
        record: None,
        admission: None,
    };
    this.summary.timeline.started_at = Some(200);
    let runs = [earlier, this.summary.clone()];
    this.summary.state = JobState::Completed;
    this.summary.conclusion = Some("failure".to_owned());
    this.summary.timeline.finished_at = Some(300);
    let compared = this.compare(&runs, 400);
    assert_eq!((compared.failed, compared.finished), (1, 2));
    assert_eq!(compared.median_ran, Some(60), "the lower of 60 and 100");
}

#[test]
fn runs_another_machine_ran_do_not_set_this_jobs_waits() {
    let ran_here = |id: u64, state: JobState, started: u64| JobSummary {
        id,
        conclusion: Some("success".to_owned()),
        ..timed(
            state,
            Timeline {
                queued_at: 100,
                started_at: Some(started),
                finished_at: Some(started + 60),
                ..Timeline::default()
            },
        )
    };
    let this = Job {
        summary: ran_here(7, JobState::Completed, 130),
        steps: vec![],
        record: None,
        admission: None,
    };
    let runs = [
        ran_here(5, JobState::Elsewhere, 100),
        ran_here(6, JobState::Elsewhere, 101),
        this.summary.clone(),
    ];
    let compared = this.compare(&runs, 400);
    assert_eq!(compared.finished, 1);
    assert_eq!(compared.median_waited, Some(30));
}

#[test]
fn steps_that_meet_at_a_second_share_no_sample() {
    let series = SampleSeries {
        job: 7,
        interval: 2,
        samples: vec![
            sample(100, 1, 1000),
            sample(102, 5, 3000),
            sample(104, 2, 0),
        ],
    };
    let first = series.usage_between(98, 102);
    let second = series.usage_between(102, 104);
    assert_eq!(first.map(|usage| usage.memory_peak), Some(5));
    assert_eq!(second.map(|usage| usage.memory_peak), Some(2));
    assert_eq!(series.usage_between(104, 105), None);
}

#[test]
fn a_job_whose_name_is_not_known_yet_is_named_by_its_id() {
    let mut unnamed = job(JobState::Running, None);
    unnamed.name.clear();
    assert_eq!(unnamed.title().to_string(), "CI / job 7");
    unnamed.workflow = None;
    assert_eq!(unnamed.title().to_string(), "job 7");
}
