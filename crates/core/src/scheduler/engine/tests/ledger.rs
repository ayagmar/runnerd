//! The jobs the engine follows, queued or closing: which writes move a job from one stage to the other, which leave
//! it, and what a read whose answer lands after a pass moved its job does.

use std::time::Duration;

use super::records::{FAILED, recording};
use super::*;
use crate::scheduler::engine::history::LOST_AFTER;

/// Makes the capture of the runner in `slot` wait until the test says which job it took, as a real runner's waits
/// for the job GitHub gives it.
fn capture_waits(github: &FakeGithub, docker: &FakeDocker, slot: usize) {
    let mut state = docker.0.lock().unwrap();
    state
        .container_captures
        .remove(&format!("container-{slot}"));
    state.captures_wait = Some(github.clone());
    state.capture_end = CaptureEnd::Container;
}

/// The runner in `slot` reports that it took `job`.
fn reports(docker: &FakeDocker, slot: usize, job: u64) {
    docker
        .0
        .lock()
        .unwrap()
        .container_captures
        .insert(format!("container-{slot}"), format!("@@job {job} {job}\n"));
}

/// The runner in `slot` and its container go, as `docker rm -f` and a deregistration do.
fn runner_goes(github: &FakeGithub, docker: &FakeDocker, slot: usize) {
    let container = format!("container-{slot}");
    docker
        .0
        .lock()
        .unwrap()
        .containers
        .retain(|candidate| candidate.id != container);
    let name = identity("ci", ID_A).runner_name("app", slot, 1);
    github
        .0
        .lock()
        .unwrap()
        .runners
        .retain(|runner| runner.name != name);
}

/// Waits until a runner's report that it took `job` reached the engine.
async fn until_runs(engine: &mut Engine<FakeGithub, FakeDocker>, job: u64) {
    for _ in 0..1000 {
        engine.take_found();
        if engine.occupants_mut().any(|occupant| occupant.runs(job)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no runner reported job {job}");
}

async fn state_now(handle: &Handle, job: u64) -> Option<JobState> {
    handle.store.flush().await;
    handle
        .store
        .job(job)
        .await
        .unwrap()
        .map(|job| job.summary.state)
}

/// A read of a job followed as queued may land after a pass closed the job: its runner reported it and stopped
/// while GitHub was slow to answer. Whatever the answer says, the job stays followed as closing, and the same tick
/// still asks GitHub whether it completed.
#[tokio::test]
async fn a_read_of_a_queued_job_that_lands_after_a_pass_closed_it_leaves_it_closing() {
    for completed in [true, false] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        running_job(&github, &docker, "o/r", 1, 7);
        capture_waits(&github, &docker, 1);
        github.0.lock().unwrap().queued = vec![job(7)];
        let (mut engine, handle, _dir) = engine(&github, &docker);
        engine.recover().await.unwrap();
        engine.tick(Shutdown::No).await;
        assert!(engine.is_followed_as_queued(7), "job 7 is listed queued");

        // The next listing no longer shows it, so the tick reads it, and GitHub holds the answer.
        github.0.lock().unwrap().queued.clear();
        github.hold_jobs();
        let exits = engine.exits.clone();
        let pass = async {
            github.until_waiting(1).await;
            // Meanwhile its runner reports it and stops, and a pass releases the runner.
            reports(&docker, 1, 7);
            until_state(&handle, 7, JobState::Running).await;
            for container in &mut docker.0.lock().unwrap().containers {
                container.running = false;
            }
            exits.stopped("container-1".to_owned());
            tokio::time::timeout(Duration::from_secs(60), async {
                while !docker.calls().contains(&"remove container-1".to_owned()) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the pass released the runner");
            {
                let mut state = github.0.lock().unwrap();
                if completed {
                    state
                        .jobs
                        .insert(7, details(7, "completed", Some("success")));
                } else {
                    state.gone_jobs.insert(7);
                }
            }
            github.release_jobs();
        };
        tokio::join!(engine.tick(Shutdown::No), pass);

        if completed {
            assert_eq!(
                state_now(&handle, 7).await,
                Some(JobState::Completed),
                "the tick read job 7 as closing too"
            );
        } else {
            assert!(engine.is_closing(7), "job 7 is still followed as closing");
            assert_eq!(state_now(&handle, 7).await, Some(JobState::Running));
        }
    }
}

/// Runners in slots 1 and 2 of `o/r`, both running job 1, as when GitHub hands one job to two runners: slot 1's
/// capture reports it at once, slot 2's when the test says.
async fn twins(
    github: &FakeGithub,
    docker: &FakeDocker,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    running_job(github, docker, "o/r", 1, 1);
    running_job(github, docker, "o/r", 2, 1);
    capture_waits(github, docker, 2);
    let (mut engine, handle, dir) = engine(github, docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    (engine, handle, dir)
}

/// A runner that reports a job whose first runner already went leaves the job closing: noting what a runner took
/// stops following a job only as queued. While that runner runs, the job is read both as its running job and as
/// closing, each tick.
#[tokio::test]
async fn a_twin_runner_reporting_a_job_whose_first_runner_went_leaves_it_closing() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = twins(&github, &docker).await;
    runner_goes(&github, &docker, 1);
    engine.tick(Shutdown::No).await;
    assert!(engine.is_closing(1), "job 1's first runner went");

    reports(&docker, 2, 1);
    until_runs(&mut engine, 1).await;
    assert!(engine.is_closing(1), "job 1 is still followed as closing");
    let before = reads_of(&github, 1);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        reads_of(&github, 1) - before,
        2,
        "job 1 is read as slot 2's running job and as closing"
    );
}

/// A runner that ran a job already closing, as its twin did, starts the wait for GitHub's word over when it goes:
/// the job is lost `LOST_AFTER` after its last runner went, not its first.
#[tokio::test]
async fn a_twin_runner_going_starts_the_closing_jobs_wait_over() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, handle, _dir) = twins(&github, &docker).await;
    reports(&docker, 2, 1);
    until_runs(&mut engine, 1).await;
    runner_goes(&github, &docker, 1);
    engine.tick(Shutdown::No).await;
    assert!(engine.is_closing(1), "job 1's first runner went");

    engine.age_followed_jobs(LOST_AFTER);
    runner_goes(&github, &docker, 2);
    engine.tick(Shutdown::No).await;
    assert!(engine.is_closing(1), "job 1's last runner went just now");
    assert_eq!(state_now(&handle, 1).await, Some(JobState::Running));
}

/// After a restart, a job whose runner's container went is closing, and is lost `LOST_AFTER` after the restart: the
/// wait before it does not survive.
#[tokio::test(start_paused = true)]
async fn a_restart_closes_a_job_whose_runner_went_from_the_restart_on() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_job(&github, &docker, "o/r", 1, 1);
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    runner_goes(&github, &docker, 1);
    engine.tick(Shutdown::No).await;
    assert!(engine.is_closing(1), "job 1's runner went");
    drop((engine, handle));

    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    assert!(engine.is_closing(1), "the history holds job 1 open");
    engine.tick(Shutdown::No).await;
    assert_eq!(state_now(&handle, 1).await, Some(JobState::Running));
    tokio::time::advance(LOST_AFTER).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(state_now(&handle, 1).await, Some(JobState::Lost));
}

/// A left runner whose worker log, read late, names a job already closing starts the job's wait over, as its
/// runner going would.
#[tokio::test]
async fn a_left_runner_whose_worker_log_names_a_closing_job_starts_its_wait_over() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    {
        // A stopped runner of the last daemon, whose files Docker cannot read yet: its worker log names job 1.
        let mut state = docker.0.lock().unwrap();
        state.containers = vec![runner_container(2, 8, false)];
        state
            .container_logs
            .insert("container-2".to_owned(), FAILED.to_owned());
        state
            .diag
            .insert("container-2".to_owned(), docker::tests::diag_archive(1));
        state.fail_files = u32::MAX;
    }
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    let dir = TestDir::new("ledger-claim");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    // The history has job 1 running on a runner whose container is gone.
    handle.store.queued(
        "o/r",
        &QueuedJob {
            id: 1,
            run_id: 1,
            name: "build".to_owned(),
            ..QueuedJob::default()
        },
        crate::time::now_ms(),
    );
    handle
        .store
        .claimed(1, 1, "runnerd-gone", crate::time::now_ms());
    handle.store.flush().await;
    engine.recover().await.unwrap();
    assert!(
        engine.is_closing(1),
        "job 1's runner went with the last daemon"
    );
    assert_eq!(engine.state_counts().unread, 1, "its files were not read");

    engine.age_followed_jobs(LOST_AFTER);
    docker.0.lock().unwrap().fail_files = 0;
    engine.make_unread_due();
    engine.tick(Shutdown::No).await;
    assert!(
        engine.is_closing(1),
        "the left runner's claim restarted the wait"
    );
    assert_eq!(state_now(&handle, 1).await, Some(JobState::Running));
}

/// A job that left the queue and that GitHub fails to read keeps its clock when a listing that began before a reload
/// removed its repository shows it again: GitHub has failed its reads all along, so it closes as run elsewhere.
#[tokio::test]
async fn a_listing_of_a_removed_repository_keeps_the_clock_of_a_job_github_fails_to_read() {
    let github = FakeGithub::default();
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(5)];
        state.queued_on.insert(5, "o/s".to_owned());
        state.failed_jobs.insert(5);
    }
    let dir = TestDir::new("ledger-unanswered");
    let (mut engine, handle) =
        engine_with(&github, &FakeDocker::default(), &dir, &two_repositories());
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert!(engine.is_followed_as_queued(5), "job 5 is listed queued");

    // It leaves the listing, and GitHub fails to read it.
    github.0.lock().unwrap().queued.clear();
    engine.tick(Shutdown::No).await;
    assert!(
        engine.is_followed_as_queued(5),
        "one failed read is no answer"
    );
    engine.age_followed_jobs(LOST_AFTER);

    // GitHub lists it again while a reload removes `o/s`.
    github.0.lock().unwrap().queued = vec![job(5)];
    github.hold_queued("o/s");
    let reload = async {
        github.until_waiting(1).await;
        let committed = handle
            .control
            .replace_config(
                Config::parse(CONFIG).unwrap(),
                None,
                None,
                Commit::new(|| Ok(None)),
                RunningSlots::Drain,
                Author::Owner,
            )
            .await;
        assert_eq!(committed, Ok(Vec::new()));
        github.release_queued();
    };
    tokio::join!(engine.tick(Shutdown::No), reload);
    assert!(
        !engine.is_followed_as_queued(5),
        "GitHub failed every read of job 5 for LOST_AFTER"
    );
    assert_eq!(state_now(&handle, 5).await, Some(JobState::Elsewhere));
}
