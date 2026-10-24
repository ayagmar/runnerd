//! Warm runners on the fakes (ADR 0050): a registered runner waits for jobs of an active repository, a queued job
//! takes it, it gives its room to another repository's job, a run's expected jobs get runners before GitHub queues
//! them, a quiet repository's warm runners stop for good, and a reseed still runs.

use super::*;
use crate::scheduler::IDLE_TIMEOUT;
use crate::scheduler::warm::Learned;
use crate::time;

const MINUTE: u64 = 60 * 1000;

/// `CONFIG` with its default `warm`.
fn warm_config() -> String {
    CONFIG.replace("warm = 0\n", "")
}

/// A job of `o/r`'s `CI` workflow, of run `run`, queued at `at` (ms).
fn ci_job(id: u64, run: u64, at: u64) -> QueuedJob {
    QueuedJob {
        run_id: run,
        workflow: Some("CI".to_owned()),
        created_at: at,
        ..job(id)
    }
}

/// An engine for `config` whose history has a job of `o/r` queued ten minutes ago, so `o/r` is active.
async fn active(
    github: &FakeGithub,
    docker: &FakeDocker,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    let dir = TestDir::new("engine");
    let (engine, handle) = engine_with(github, docker, &dir, config);
    handle
        .store
        .queued("o/r", &ci_job(1, 1, time::now_ms() - 10 * MINUTE));
    handle.store.flush().await;
    (engine, handle, dir)
}

/// The repository of each runner, and whether it is warm, by slot.
fn slots(handle: &Handle) -> Vec<Option<(String, bool)>> {
    handle.status.borrow().profiles[0]
        .slots
        .iter()
        .map(|slot| {
            slot.runner
                .as_ref()
                .map(|runner| (runner.repository.clone(), runner.warm))
        })
        .collect()
}

fn warm_in(repository: &str) -> (String, bool) {
    (repository.to_owned(), true)
}

#[tokio::test]
async fn an_active_repository_has_a_registered_idle_runner_before_any_job_is_queued() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(registered(&github.calls()).len(), 1);
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert!(handle.status.borrow().queued.is_empty(), "no job is queued");

    // Not one more on the next tick: it is still there.
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
}

#[tokio::test]
async fn a_repository_without_jobs_gets_no_warm_runner() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &warm_config());
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn warm_zero_starts_no_warm_runner_however_active() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, _dir) = active(&github, &docker, CONFIG).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn a_queued_job_takes_the_warm_runner_and_another_one_starts_for_the_next_job() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let warm_id = github.0.lock().unwrap().runners[0].id;

    // GitHub hands the job to the warm runner as soon as it is queued, before runnerd polls.
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(5)];
        state.assign = Some((5, warm_id));
    }
    engine.tick(Shutdown::No).await;
    // The next poll lists the runner busy.
    engine.tick(Shutdown::No).await;
    engine.publish();
    // No runner for the job; one warm runner for the jobs to come.
    assert_eq!(registered(&github.calls()).len(), 2);
    assert_eq!(
        slots(&handle),
        [Some(("o/r".to_owned(), false)), Some(warm_in("o/r"))]
    );
}

#[tokio::test]
async fn a_job_queued_while_the_warm_runner_waits_counts_on_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let config = warm_config().replace("slots = 2", "slots = 1");
    let (mut engine, handle, _dir) = active(&github, &docker, &config).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    github.0.lock().unwrap().queued = vec![job(5)];
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(registered(&github.calls()).len(), 1, "no second runner");
    assert_eq!(waiting(&handle), [Some(api::Waiting::RunnerStarting)]);
    assert_eq!(
        slots(&handle),
        [Some(("o/r".to_owned(), false))],
        "it waits for that job now, not warm"
    );
}

/// `o/r` and `o/s` sharing `app`'s one slot; `o/s` has warm runners off and runs jobs labelled `other`.
fn shared_slot() -> String {
    format!(
        "{}\n[[repositories]]\nname = \"o/s\"\nprofile = \"app\"\nlabels = [\"self-hosted\", \"other\"]\nwarm = 0\n",
        warm_config().replace("slots = 2", "slots = 1")
    )
}

#[tokio::test]
async fn a_warm_runner_gives_its_slot_to_a_queued_job_of_another_repository() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &shared_slot()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r"))]);
    let warm_id = github.0.lock().unwrap().runners[0].id;

    github.0.lock().unwrap().queued = vec![QueuedJob {
        labels: vec!["self-hosted".to_owned(), "other".to_owned()],
        ..job(7)
    }];
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert!(
        github.calls().contains(&format!("delete {warm_id}")),
        "the warm runner is deleted: {:?}",
        github.calls()
    );
    assert_eq!(slots(&handle), [Some(("o/s".to_owned(), false))]);
    assert_eq!(registered(&github.calls()).len(), 2);
}

#[tokio::test]
async fn a_warm_runner_that_took_a_job_before_its_stop_keeps_it_and_the_other_job_waits() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &shared_slot()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![QueuedJob {
            labels: vec!["self-hosted".to_owned(), "other".to_owned()],
            ..job(7)
        }];
        // GitHub refuses the delete: the warm runner just took a job of o/r.
        state.busy_deletes = 1;
    }
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "no runner for o/s yet"
    );
    assert_eq!(slots(&handle), [Some(("o/r".to_owned(), false))]);
}

#[tokio::test]
async fn a_runs_expected_jobs_get_runners_before_github_queues_them() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let config = warm_config().replace("slots = 2", "slots = 4");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    // Three runs of CI two days ago, three jobs each: o/r is quiet now.
    let then = time::now_ms() - 2 * 24 * 60 * MINUTE;
    for run in 1..=3 {
        for n in 0..3 {
            let id = run * 10 + n;
            handle
                .store
                .queued("o/r", &ci_job(id, run, then + run * MINUTE));
        }
    }
    handle.store.flush().await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new(), "quiet");

    // The first job of a new run: a runner for it, two for the run's other jobs and one warm.
    github.0.lock().unwrap().queued = vec![ci_job(100, 50, time::now_ms())];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4);
}

#[tokio::test]
async fn warm_runners_start_no_more_than_the_budget_holds() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let config = warm_config().replace("slots = 2", "slots = 4");
    let (mut engine, _handle, _dir) = active(&github, &docker, &config).await;
    // Two 4G caps, with the mirror off.
    engine.memory_max = Some(9 << 30);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![job(5)];
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(registered(&github.calls()).len(), 2);
    assert!(engine.memory_in_use() <= 9 << 30, "within MemoryMax");
}

#[tokio::test]
async fn warm_runners_stop_once_the_repository_is_quiet_and_do_not_come_back() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);

    // An hour and more later: no job since, and the runner waited past IDLE_TIMEOUT.
    engine.warm.learned = Some((Learned::default(), std::time::SystemTime::now()));
    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.started_at -= IDLE_TIMEOUT;
    }
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [None, None]);
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(registered(&github.calls()).len(), 1, "none starts again");
}

#[tokio::test]
async fn a_warm_runner_stops_for_its_profiles_reseed_which_runs() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle)[1], Some(warm_in("o/r")));

    // The job in slot 1 ends: slot 1's layers may become the snapshot once no runner uses the profile.
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    let seed = format!("start {}", Names::new("runnerd").seed("app"));
    for _ in 0..100 {
        if docker.calls().contains(&seed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(docker.calls().contains(&seed), "{:?}", docker.calls());
    engine.publish();
    assert_eq!(slots(&handle), [None, None]);
}

#[tokio::test]
async fn an_idle_runner_adopted_after_a_restart_stays_as_the_warm_runner() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    idle_runner(&github, &docker);
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
    assert!(
        !github
            .calls()
            .iter()
            .any(|call| call.starts_with("delete ")),
        "it is not deleted: {:?}",
        github.calls()
    );
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
}
