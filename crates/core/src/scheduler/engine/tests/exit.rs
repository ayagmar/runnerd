//! What each way a runner goes concludes: that it ran a job, that its job ended, that it may have taken a listed job,
//! and what its pool makes of it. Unread logs are their own case: nothing is known, so they neither count against a
//! pool nor clear what holds it.

use std::time::{Duration, SystemTime};

use super::*;
use crate::api::HoldCause;
use crate::scheduler::engine::history::{Capture, Found};
use crate::scheduler::engine::runners::Removal;
use crate::scheduler::warm::Learned;
use crate::scheduler::{Hold, IDLE_TIMEOUT, SlotRef, StopReason};
use crate::time;

fn first_slot() -> SlotRef {
    SlotRef {
        profile: "app".to_owned(),
        index: 0,
    }
}

/// What an exit left noted of its runner.
#[derive(Debug, PartialEq, Eq)]
struct Concluded {
    ran: bool,
    job_ended: bool,
    claimed: usize,
}

fn concluded(engine: &Engine<FakeGithub, FakeDocker>) -> Concluded {
    Concluded {
        ran: engine.measure_is_due("app"),
        job_ended: engine.warm.active("o/r", time::now_ms()),
        claimed: engine.unnamed_claims("o/r"),
    }
}

/// `o/r` with a runner just started in slot 1, listed idle, that no capture has named a job for.
async fn engine_with_a_started_runner(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    github.0.lock().unwrap().queued = vec![job(5)];
    let (mut engine, handle) = engine_with(github, docker, dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    // Starting one noted the repository active: only what the exit notes is left to see.
    engine.warm.testing_learned(Learned::default());
    let before = concluded(&engine);
    assert!(
        !before.ran && !before.job_ended && before.claimed == 0,
        "nothing is concluded yet: {before:?}"
    );
    (engine, handle)
}

#[tokio::test]
async fn a_release_concludes_from_its_log_and_what_a_listing_showed() {
    struct Case {
        what: &'static str,
        logs: Option<&'static str>,
        listed_busy: bool,
        waited: bool,
        then: Concluded,
    }
    let cases = [
        Case {
            what: "its log shows a job",
            logs: Some("Running job: build\n"),
            listed_busy: false,
            waited: false,
            then: Concluded {
                ran: true,
                job_ended: true,
                claimed: 1,
            },
        },
        Case {
            what: "it connected and took none",
            logs: Some("Listening for Jobs\n"),
            listed_busy: false,
            waited: false,
            then: Concluded {
                ran: false,
                job_ended: false,
                claimed: 1,
            },
        },
        Case {
            what: "it waited out its time, and its log no longer shows it connected",
            logs: Some("session lost\n"),
            listed_busy: false,
            waited: true,
            then: Concluded {
                ran: false,
                job_ended: false,
                claimed: 1,
            },
        },
        Case {
            what: "it never connected",
            logs: Some("session lost\n"),
            listed_busy: false,
            waited: false,
            then: Concluded {
                ran: false,
                job_ended: false,
                claimed: 0,
            },
        },
        Case {
            what: "a listing showed it busy, though its log shows no job",
            logs: Some("session lost\n"),
            listed_busy: true,
            waited: false,
            then: Concluded {
                ran: true,
                job_ended: true,
                claimed: 0,
            },
        },
        Case {
            what: "its log could not be read",
            logs: None,
            listed_busy: false,
            waited: false,
            then: Concluded {
                ran: true,
                job_ended: false,
                claimed: 1,
            },
        },
    ];
    for case in cases {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        let dir = TestDir::new("exit-released");
        let (mut engine, _handle) = engine_with_a_started_runner(&github, &docker, &dir).await;
        let mut copy = engine.occupant(&first_slot()).unwrap().clone();
        if case.listed_busy {
            copy.set_listed_busy();
        }
        if case.waited {
            copy.started_at -= IDLE_TIMEOUT;
        }

        engine.released(&first_slot(), &copy, case.logs, Ok(Removal::Done));

        assert_eq!(concluded(&engine), case.then, "{}", case.what);
    }
}

/// A log Docker fails to give says nothing of the pool: counted as a runner that connected and left, it would hold a
/// healthy pool; counted as one that took a job, it would clear a broken pool's backoff.
#[tokio::test]
async fn a_log_docker_fails_to_give_leaves_its_pools_streaks_and_hold_alone() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("exit-unread");
    let (mut engine, _handle) = engine_with_a_started_runner(&github, &docker, &dir).await;
    let held = Hold {
        until: SystemTime::now() + Duration::from_hours(1),
        cause: HoldCause::Start,
    };
    engine.set_streaks("o/r", "app", 2, 1);
    engine.set_held("o/r", "app", held);
    {
        let mut state = docker.0.lock().unwrap();
        state.fail_logs = 1;
        state.containers[0].running = false;
    }

    engine.tick(Shutdown::No).await;

    assert!(
        engine.occupant(&first_slot()).is_none(),
        "its runner has gone"
    );
    let facts = engine.pool_hold("o/r", "app").unwrap();
    assert_eq!(facts.failures, 2, "a failure streak stays as it was");
    assert_eq!(facts.idle_exits, 1, "it is no idle exit");
    assert_eq!(
        facts.held.map(|held| (held.until, held.cause)),
        Some((held.until, held.cause)),
        "the hold stays as it was"
    );
}

#[tokio::test]
async fn a_killed_runner_ran_and_ended_a_job_only_if_it_was_busy() {
    for (listed_busy, then) in [
        (
            true,
            Concluded {
                ran: true,
                job_ended: true,
                claimed: 0,
            },
        ),
        (
            false,
            Concluded {
                ran: false,
                job_ended: false,
                claimed: 0,
            },
        ),
    ] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        let dir = TestDir::new("exit-killed");
        let (mut engine, _handle) = engine_with_a_started_runner(&github, &docker, &dir).await;
        if listed_busy {
            engine.occupants_mut().next().unwrap().set_listed_busy();
        }

        engine.kill(&first_slot()).await;

        assert_eq!(concluded(&engine), then, "listed busy: {listed_busy}");
    }
}

/// A runner removed as idle after GitHub agreed had no job to end, though adoption counts one busy until a listing
/// says otherwise. Its slot may still hold what a job left, so it is measured.
#[tokio::test]
async fn an_adopted_runner_removed_as_idle_is_measured_but_ended_no_job() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("exit-removed-idle");
    let (mut engine, _handle) = engine_with_a_started_runner(&github, &docker, &dir).await;
    engine.occupants_mut().next().unwrap().set_listed_busy();

    engine
        .remove_idle(vec![(first_slot(), StopReason::Shutdown)])
        .await;

    assert!(
        engine.occupant(&first_slot()).is_none(),
        "GitHub agreed, so its runner has gone"
    );
    assert_eq!(
        concluded(&engine),
        Concluded {
            ran: true,
            job_ended: false,
            claimed: 0,
        }
    );
}

/// A log capture that reports `job` only once the runner's container has gone, as one does that reads to its end.
fn capture_names_a_job_once_its_container_went(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    docker: &FakeDocker,
    job: u64,
) {
    let occupant = engine.occupant(&first_slot()).unwrap().clone();
    let (found, docker) = (engine.found_sender(), docker.clone());
    let runner = occupant.runner_name.clone();
    let capture = tokio::spawn(async move {
        while docker
            .0
            .lock()
            .unwrap()
            .containers
            .iter()
            .any(|container| container.id == occupant.container)
        {
            tokio::task::yield_now().await;
        }
        found.send(Found::named(runner, job)).unwrap();
        Capture::Done
    });
    engine.set_capture(
        &engine.occupant(&first_slot()).unwrap().runner_name.clone(),
        capture,
    );
}

/// What it ran stays in slot 1, so a forced stop that read the runner before its capture ended must still measure it.
#[tokio::test]
async fn a_job_named_as_a_forced_stop_ends_its_capture_counts_as_run() {
    for kill in [true, false] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        let dir = TestDir::new("exit-after-capture");
        let (mut engine, _handle) = engine_with_a_started_runner(&github, &docker, &dir).await;
        capture_names_a_job_once_its_container_went(&mut engine, &docker, 5);

        if kill {
            engine.kill(&first_slot()).await;
        } else {
            engine
                .remove_idle(vec![(first_slot(), StopReason::Shutdown)])
                .await;
        }

        assert!(
            engine.occupant(&first_slot()).is_none(),
            "its runner has gone (killed: {kill})"
        );
        assert!(
            engine.measure_is_due("app"),
            "slot 1 ran the job its capture named, so it is measured (killed: {kill})"
        );
    }
}
