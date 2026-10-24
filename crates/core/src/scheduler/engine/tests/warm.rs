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

/// Waits for the check of the shared caches to end.
async fn check_ends(engine: &mut Engine<FakeGithub, FakeDocker>) {
    while engine.trim.as_ref().is_some_and(|trim| !trim.is_finished()) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
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
    // The start's check of the shared caches, which warm runners wait for.
    check_ends(&mut engine).await;

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

#[tokio::test]
async fn the_shared_caches_are_checked_hourly_though_a_warm_runner_always_waits() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert!(engine.trim.is_none(), "no check while a runner runs");

    // An hour after the last check, with a job ended since: the warm runner stops for the check.
    engine.checked_at -= Duration::from_secs(3600);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [None, None]);
    assert!(engine.trim.is_some(), "the shared caches are checked");

    // Docker takes long to measure large volumes: until the check ends, no runner mounts what it may remove.
    let check = engine.trim.replace(tokio::spawn(std::future::pending()));
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    engine.publish();
    assert_eq!(slots(&handle), [None, None], "none while the check runs");
    assert_eq!(registered(&github.calls()).len(), 1);

    // It ended: the warm runner comes back.
    if let Some(pending) = engine.trim.take() {
        pending.abort();
    }
    engine.trim = check;
    check_ends(&mut engine).await;
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert_eq!(registered(&github.calls()).len(), 2);
}

#[tokio::test]
async fn a_warm_runner_that_took_a_job_before_its_stop_leaves_no_room_for_younger_jobs_either() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    // o/r's 4G warm runner in `app`; o/s runs 1G jobs in `small`. 4.5G leaves room for o/s's jobs only once the
    // warm runner stops.
    let config = format!(
        "{}\n[profiles.small]\nimage_stage = \"app\"\ncpus = 1\nmemory = \"1G\"\nslots = 2\n\n\
         [[repositories]]\nname = \"o/s\"\nprofile = \"small\"\nlabels = [\"self-hosted\", \"other\"]\nwarm = 0\n",
        warm_config().replace("slots = 2", "slots = 1")
    );
    let (mut engine, _handle, _dir) = active(&github, &docker, &config).await;
    engine.memory_max = Some(4608 << 20);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    {
        let mut state = github.0.lock().unwrap();
        state.queued = [7, 8]
            .into_iter()
            .map(|id| QueuedJob {
                labels: vec!["self-hosted".to_owned(), "other".to_owned()],
                ..job(id)
            })
            .collect();
        // The warm runner just took a job of o/r: GitHub refuses its delete.
        state.busy_deletes = 1;
    }
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "neither o/s job starts on room the warm runner still holds"
    );
    assert!(engine.memory_in_use() <= 4608 << 20, "within MemoryMax");
}

#[tokio::test]
async fn a_warm_runner_gone_without_a_job_after_waiting_holds_back_nothing() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;

    // Docker restarted under it an hour later: its container ended, and it took no job.
    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.started_at -= Duration::from_secs(3600);
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.runner_log = Some("Listening for Jobs".to_owned());
        state.containers[0].running = false;
    }
    github.0.lock().unwrap().queued = vec![job(5)];
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(waiting(&handle), [Some(api::Waiting::RunnerStarting)]);
    assert_eq!(
        registered(&github.calls()).len(),
        3,
        "a runner for the job, and a warm one"
    );
}

#[tokio::test]
async fn runners_failing_together_hold_their_repository_back_once() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![job(1), job(2)];
    engine.tick(Shutdown::No).await;
    {
        let mut state = docker.0.lock().unwrap();
        state.runner_log = Some("Failed to connect".to_owned());
        for container in &mut state.containers {
            container.running = false;
        }
    }
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine.repositories["o/r"].failures, 1,
        "two runners of one start fail once"
    );
}

#[tokio::test]
async fn a_warm_runner_leaving_slot_1_without_a_job_needs_no_reseed() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.warm.learned = Some((Learned::default(), std::time::SystemTime::now()));
    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.started_at -= IDLE_TIMEOUT;
    }
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    let seed = format!("start {}", Names::new("runnerd").seed("app"));
    assert!(
        !docker.calls().contains(&seed),
        "slot 1 changed nothing: {:?}",
        docker.calls()
    );
}

#[tokio::test]
async fn a_profile_with_more_cpus_than_the_budget_still_gets_a_warm_runner() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let config = warm_config().replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
    );
    let (mut engine, handle, _dir) = active(&github, &docker, &config).await;
    // One CPU for a profile of two, as a job's runner would reserve.
    engine.cpus = Some(1000);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
}

#[tokio::test]
async fn a_run_under_way_before_a_restart_counts_from_its_jobs_not_from_the_restart() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let config = warm_config().replace("slots = 2", "slots = 4");
    // Run 50's job, queued half an hour ago, runs on a runner the restart adopts.
    running_job(&github, &docker, "o/r", 1, 50);
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .get_mut(&50)
        .unwrap()
        .workflow_name = Some("CI".to_owned());
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    // Three runs of CI two days ago, three jobs each, queued together.
    let then = time::now_ms() - 2 * 24 * 60 * MINUTE;
    for run in 1..=3 {
        for n in 0..3 {
            handle
                .store
                .queued("o/r", &ci_job(run * 10 + n, run, then + run * MINUTE));
        }
    }
    let queued_at = time::now_ms() - 30 * MINUTE;
    handle.store.queued("o/r", &ci_job(50, 50, queued_at));
    handle.store.claimed(
        50,
        50,
        &identity("ci", ID_A).runner_name("app", 1, 1),
        queued_at + MINUTE,
    );
    handle.store.flush().await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    // Its run stopped counting 25 minutes ago: one warm runner, as o/r is active, and none for the run's other jobs.
    assert_eq!(registered(&github.calls()).len(), 1);
}

#[tokio::test]
async fn warm_runners_that_keep_dying_while_they_wait_hold_their_repository_back() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let config = warm_config().replace("slots = 2", "slots = 1");
    let (mut engine, _handle, _dir) = active(&github, &docker, &config).await;
    engine.recover().await.unwrap();
    docker.0.lock().unwrap().runner_log = Some("Listening for Jobs".to_owned());
    for exits in 1..=2 {
        engine.tick(Shutdown::No).await;
        // Something kills each one after it waited a while.
        for occupant in engine.slots.values_mut().flatten().flatten() {
            occupant.started_at -= Duration::from_secs(3600);
        }
        for container in &mut docker.0.lock().unwrap().containers {
            container.running = false;
        }
        engine.tick(Shutdown::No).await;
        assert_eq!(
            engine.repositories["o/r"].held_until.is_some(),
            exits == 2,
            "held after the second in a row, not the first"
        );
    }
    assert_eq!(registered(&github.calls()).len(), 2, "no third");
}

#[tokio::test]
async fn a_warm_runner_that_waited_out_its_time_ends_the_failure_streak_before_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    // A failure days ago, and no job since to end its streak.
    engine.repositories.get_mut("o/r").unwrap().failures = 1;

    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.started_at -= IDLE_TIMEOUT;
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.runner_log = Some("Listening for Jobs".to_owned());
        for container in &mut state.containers {
            container.running = false;
        }
    }
    engine.tick(Shutdown::No).await;

    assert_eq!(
        engine.repositories["o/r"].failures, 0,
        "the next failure is announced as a new streak's first"
    );
}

#[tokio::test]
async fn a_warm_runner_whose_labels_or_limits_a_reload_changed_makes_way_for_one_that_has_them() {
    let gpu = warm_config().replace(
        "labels = [\"self-hosted\", \"app\"]",
        "labels = [\"self-hosted\", \"app\", \"gpu\"]",
    );
    let more_memory = warm_config().replace("memory = \"4G\"", "memory = \"6G\"");
    for config in [gpu, more_memory] {
        let github = FakeGithub::default();
        let docker = FakeDocker::default();
        let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
        engine.recover().await.unwrap();
        engine.tick(Shutdown::No).await;
        let before = runner_ids(&handle, &mut engine);
        assert_eq!(before.len(), 1, "a warm runner waits");
        engine.reload(Config::parse(&config).unwrap(), None, None);
        // A job only a runner with the new labels can take, or one that needs the raised memory.
        let labels = Config::parse(&config).unwrap().repositories[0]
            .labels
            .clone();
        github.0.lock().unwrap().queued = vec![QueuedJob {
            labels,
            ..ci_job(2, 2, time::now_ms())
        }];
        engine.tick(Shutdown::No).await;
        engine.tick(Shutdown::No).await;
        let after = runner_ids(&handle, &mut engine);
        assert!(
            !after.contains(&before[0]),
            "the warm runner from before the reload is gone: {after:?}"
        );
        assert!(
            !after.is_empty(),
            "a runner with the new config waits for the job"
        );
    }
}

/// The GitHub IDs of the runners in `app`'s slots.
fn runner_ids(handle: &Handle, engine: &mut Engine<FakeGithub, FakeDocker>) -> Vec<u64> {
    engine.publish();
    handle.status.borrow().profiles[0]
        .slots
        .iter()
        .filter_map(|slot| slot.runner.as_ref().map(|runner| runner.runner_id))
        .collect()
}

#[tokio::test]
async fn a_cache_a_warm_runner_mounts_is_cleaned_and_the_runner_comes_back() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);

    // A warm runner waits all day for an active repository: it gives way to the clean.
    engine.clean_cache("runnerd-pnpm").await.unwrap();
    assert!(
        docker
            .calls()
            .contains(&"remove volume runnerd-pnpm".to_owned()),
        "{:?}",
        docker.calls()
    );
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert_eq!(registered(&github.calls()).len(), 2, "a new warm runner");
}

#[tokio::test]
async fn a_warm_runner_waits_while_a_shell_holds_back_its_profiles_reseed() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    docker.0.lock().unwrap().images = vec![records::record_image(10, 60)];
    let config = warm_config().replace("slots = 2", "slots = 2\nrecord_failures = true");
    let (mut engine, handle, _dir) = active(&github, &docker, &config).await;
    engine.recover().await.unwrap();
    engine
        .open_shell(10, docker::TtySize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // The shell mounts the snapshot, so the reseed waits for it to close, and the warm runner with it.
    engine.reseed_due.insert("app".to_owned());
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert!(
        engine.reseeds.is_empty(),
        "no reseed runs while the shell is open"
    );
}
