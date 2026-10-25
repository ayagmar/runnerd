//! What every client says a slot does, and how it counts whether CI keeps up.

use crate::api::{
    Activity, Budget, ProfileStatus, QueuedJob, RunnerStatus, SlotStatus, Status, Tone,
};

fn runner(busy: bool, warm: bool) -> RunnerStatus {
    RunnerStatus {
        repository: "o/app".to_owned(),
        name: "runnerd-jvm".to_owned(),
        runner_id: 1,
        container: "c".to_owned(),
        busy,
        started_at: 0,
        reserved: 0,
        warm,
    }
}

/// `jvm`, reseeding, with an empty slot 1, a busy, a starting and a warm runner.
fn status() -> Status {
    let slots = [
        None,
        Some(runner(true, false)),
        Some(runner(false, false)),
        Some(runner(false, true)),
    ];
    Status {
        version: "0.0.0".to_owned(),
        shutting_down: false,
        budget: Budget::default(),
        profiles: vec![ProfileStatus {
            name: "jvm".to_owned(),
            image: "runnerd-runner:base".to_owned(),
            cpus: 4,
            memory: 1 << 30,
            reseeding: true,
            slots: slots
                .into_iter()
                .enumerate()
                .map(|(index, runner)| SlotStatus {
                    slot: index + 1,
                    runner,
                })
                .collect(),
        }],
        repositories: vec![],
        queued: [100, 0, 40]
            .into_iter()
            .map(|queued_at| QueuedJob {
                id: queued_at,
                repository: "o/app".to_owned(),
                profile: None,
                run_id: 1,
                run_attempt: 1,
                workflow: None,
                name: "build".to_owned(),
                branch: None,
                url: None,
                queued_at,
                waiting: None,
            })
            .collect(),
    }
}

#[test]
fn a_runner_waiting_for_work_is_starting_or_warm_never_idle() {
    let status = status();
    let profile = &status.profiles[0];
    let lines: Vec<String> = profile
        .slots
        .iter()
        .map(|slot| Status::slot_state(profile, slot).line())
        .collect();
    assert_eq!(
        lines,
        [
            "reseeding the snapshot",
            "running a job of o/app",
            "a runner for o/app waiting for its job",
            "a warm runner for o/app, waiting for a job",
        ]
    );
    assert_eq!(
        Status::slot_state(profile, &profile.slots[2]).word(),
        "starting"
    );
}

#[test]
fn health_counts_runners_by_what_github_reports_not_by_found_job_details() {
    let health = status().health(160);
    assert_eq!(
        (health.slots, health.running, health.starting, health.warm),
        (4, 1, 1, 1)
    );
    assert_eq!(health.waiting, 3, "a job without a queue time still waits");
    assert_eq!(
        health.oldest_wait,
        Some(120),
        "the oldest known queue time, at 40"
    );
    assert_eq!(health.waiting_tone(), Tone::Warn);
}

#[test]
fn the_days_failures_are_bad_only_when_a_runner_failed() {
    let mut day = Activity {
        since: 0,
        until: 86_400,
        started: vec![],
        finished: 10,
        failed: 3,
        runner_failed: 0,
    };
    assert_eq!(day.failed_tone(), Tone::Neutral, "the jobs' own failures");
    day.runner_failed = 1;
    assert_eq!(day.failed_tone(), Tone::Bad);
}
