//! Warm runners on the fakes (ADR 0050, 0107): a registered runner waits for jobs of an active repository, a queued
//! job takes it, it gives its room to another repository's job, a run's expected jobs get runners before GitHub queues
//! them, a quiet repository's warm runners stop for good, a finished job keeps its repository active, a reload learns
//! again, restarts in a run start no more than its burst, and a reseed still runs.

use super::*;
use crate::api::HoldCause;
use crate::scheduler::warm::Learned;
use crate::scheduler::{IDLE_TIMEOUT, Pool};
use crate::time;
use std::time::SystemTime;

const MINUTE: u64 = 60 * 1000;

/// `CONFIG` with its default `warm`.
fn warm_config() -> String {
    CONFIG.replace("warm = 0\n", "")
}

/// A job of `o/r`'s `CI` workflow, of run `run`'s first attempt, queued at `at` (ms).
fn ci_job(id: u64, run: u64, at: u64) -> QueuedJob {
    QueuedJob {
        run_id: run,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        created_at: at,
        ..job(id)
    }
}

/// An engine for `config` whose history has a job of `o/r` queued five minutes ago, so `o/r` is active.
async fn active(
    github: &FakeGithub,
    docker: &FakeDocker,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    let dir = TestDir::new("engine");
    let (engine, handle) = engine_with(github, docker, &dir, config);
    handle.store.queued(
        "o/r",
        &ci_job(1, 1, time::now_ms() - 5 * MINUTE),
        time::now_ms(),
    );
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
async fn the_configured_warm_runner_started_beside_a_queued_jobs_runner_is_recorded_as_warm() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![job(5)];
    engine.tick(Shutdown::No).await;
    // The first runner waits for job 5; the second is the warm one `warm = 1` asks for, not one for a job its runs
    // are expected to queue.
    handle.store.flush().await;
    let history = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    let purposes: Vec<String> = history
        .prepare("SELECT purpose FROM runners ORDER BY started_at, name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(purposes, ["job", "warm"]);
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
async fn a_job_that_takes_a_warm_runners_memory_records_the_memory_its_stop_freed() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let config = shared_slot().replace("slots = 1", "slots = 2");
    let (mut engine, handle, _dir) = active(&github, &docker, &config).await;
    // One 4G cap, with the mirror off: a warm runner leaves 2G.
    engine.memory_max = Some(6 << 30);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);

    github.0.lock().unwrap().queued = vec![QueuedJob {
        labels: vec!["self-hosted".to_owned(), "other".to_owned()],
        ..job(7)
    }];
    engine.tick(Shutdown::No).await;
    engine.publish();
    let runner = handle.status.borrow().profiles[0]
        .slots
        .iter()
        .find_map(|slot| {
            slot.runner
                .clone()
                .filter(|runner| runner.repository == "o/s")
        })
        .unwrap();
    handle.store.flush().await;
    let admission = handle.store.runner_admitted(&runner.name).await.unwrap();
    assert_eq!(
        admission.map(|admission| admission.free),
        Some(Some(6 << 30))
    );
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
async fn a_job_a_pass_plans_but_leaves_to_the_tick_keeps_waiting() {
    // Catches a pass dropping the wait of a job it planned and did not start: only the tick takes a warm runner's room,
    // and until then the job's wait went uncounted.
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, _dir) = active(&github, &docker, &shared_slot()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let job_of_s = QueuedJob {
        labels: vec!["self-hosted".to_owned(), "other".to_owned()],
        ..job(7)
    };
    engine.set_queued("o/s", vec![job_of_s]);
    engine.wait_for_the_host(7);
    assert_eq!(
        engine.refill().await,
        0,
        "only a tick takes a warm runner's room"
    );
    assert!(
        engine.waits_for_the_host(7),
        "job 7 goes on waiting for the host's memory"
    );
}

#[tokio::test]
async fn a_start_that_failed_does_not_plan_the_tick_again() {
    // Catches a failed start counting as one that went ahead: the tick planned again for the start it deferred and
    // retried the failed one at once. Two slots, one with o/r's warm runner; o/s queues two jobs. Job 7's start
    // fails preparing its caches, and job 8's waits, as the warm runner it would replace took a job.
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let config = shared_slot().replace("slots = 1", "slots = 2");
    let (mut engine, handle, _dir) = active(&github, &docker, &config).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    {
        let mut state = github.0.lock().unwrap();
        state.queued = [7, 8]
            .into_iter()
            .map(|id| QueuedJob {
                labels: vec!["self-hosted".to_owned(), "other".to_owned()],
                ..job(id)
            })
            .collect();
        state.busy_deletes = 1;
    }
    docker.0.lock().unwrap().fail_volumes = 1;
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "no runner for o/s until the next tick"
    );
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
                .queued("o/r", &ci_job(id, run, then + run * MINUTE), time::now_ms());
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
    assert!(
        engine.count(engine.host_remembered()).plain <= 9 << 30,
        "within MemoryMax"
    );
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
    engine.warm.testing_learned(Learned::default());
    for occupant in engine.occupants_mut() {
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

/// The pool of `CONFIG`'s repository.
fn app_pool() -> Pool {
    Pool {
        repository: "o/r".to_owned(),
        profile: "app".to_owned(),
    }
}

#[tokio::test]
async fn the_history_is_read_again_once_the_last_read_is_old_and_not_before() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &warm_config());
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine.warm_targets()[&app_pool()],
        0,
        "the history is empty"
    );

    // A job the engine never saw listed, which only the history knows.
    handle.store.queued(
        "o/r",
        &ci_job(1, 1, time::now_ms() - 5 * MINUTE),
        time::now_ms(),
    );
    handle.store.flush().await;
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine.warm_targets()[&app_pool()],
        0,
        "a read inside REREAD stands, so the history is not read every tick"
    );

    engine.warm.testing_age_read();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine.warm_targets()[&app_pool()],
        1,
        "the read past REREAD learned the job"
    );
}

#[tokio::test]
async fn a_read_of_the_history_that_fails_keeps_what_was_learned() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(engine.warm_targets()[&app_pool()], 1, "o/r is active");

    // The next read finds no table to read.
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .execute_batch("ALTER TABLE jobs RENAME TO jobs_gone")
        .unwrap();
    engine.warm.testing_age_read();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine.warm_targets()[&app_pool()],
        1,
        "one failed read drops no warm runner"
    );
}

#[tokio::test]
async fn a_warm_runner_stops_for_its_profiles_reseed_once_slot_1_changed_reseed_at() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    // 2G, the default `reseed_at`.
    docker.0.lock().unwrap().measured = Some(format!("slot 1 changed {} bytes\n", 2u64 << 30));
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle)[1], Some(warm_in("o/r")));

    // The job in slot 1 ends: slot 1's layers become the snapshot once no runner uses the profile.
    docker.0.lock().unwrap().containers[0].running = false;
    let seed = format!("start {}", Names::new("runnerd").seed("app"));
    until_called(&mut engine, &docker, &seed).await;
    engine.publish();
    assert_eq!(slots(&handle), [None, None]);
}

/// `o/r`, active, with job 1 in slot 1 and its warm runner waiting in slot 2; then job 1 ends, and the measure of
/// slot 1 prints `measured`. Ticks until that measure ran and was taken in.
async fn slot_1_job_ends_and_measures(
    github: &FakeGithub,
    docker: &FakeDocker,
    measured: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    running_job(github, docker, "o/r", 1, 1);
    docker.0.lock().unwrap().measured = Some(measured.to_owned());
    let (mut engine, handle, dir) = active(github, docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle)[1], Some(warm_in("o/r")));
    docker.0.lock().unwrap().containers[0].running = false;
    let measure = format!("start {}", Names::new("runnerd").measure("app"));
    until_called(&mut engine, docker, &measure).await;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    (engine, handle, dir)
}

#[tokio::test]
async fn a_slot_1_that_changed_under_reseed_at_leaves_the_warm_runners_waiting() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    // `du` complained of a file a job deleted while it counted: the count is still the measure's.
    let measured = "du: cannot access '/workers/1/cache/upper/gradle/tmp': No such file or directory\n\
                    slot 1 changed 201011459 bytes\n";
    let (mut engine, handle, _dir) = slot_1_job_ends_and_measures(&github, &docker, measured).await;
    // A reseed would change nothing: the profile's upkeep used to stop the warm runner for it anyway, then start it
    // again.
    let seed = format!("start {}", Names::new("runnerd").seed("app"));
    assert!(!docker.calls().contains(&seed), "{:?}", docker.calls());
    engine.publish();
    assert_eq!(slots(&handle), [None, Some(warm_in("o/r"))]);
    assert_eq!(removed(&docker.calls()), ["remove container-1"]);
    assert_eq!(registered(&github.calls()).len(), 1, "nothing else starts");
}

#[tokio::test]
async fn a_requested_reseed_runs_though_slot_1_changed_under_reseed_at() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, _dir) =
        slot_1_job_ends_and_measures(&github, &docker, "slot 1 changed 1000 bytes\n").await;
    engine.request_reseed("app").unwrap();
    let seed = Names::new("runnerd").seed("app");
    until_called(&mut engine, &docker, &format!("start {seed}")).await;
    let entrypoint = docker.0.lock().unwrap().entrypoints[&seed].clone();
    assert_eq!(entrypoint[1], "0", "it reseeds whatever slot 1 changed");
}

#[tokio::test]
async fn a_profile_reseeds_as_before_when_its_last_reseed_did_not_finish_or_slot_1_cannot_be_measured()
 {
    for measured in [
        "the last reseed did not finish\n",
        "du: cannot read directory '/workers/1/cache/upper': Input/output error\n",
    ] {
        let github = FakeGithub::default();
        let docker = FakeDocker::default();
        let (mut engine, _handle, _dir) =
            slot_1_job_ends_and_measures(&github, &docker, measured).await;
        let seed = format!("start {}", Names::new("runnerd").seed("app"));
        until_called(&mut engine, &docker, &seed).await;
    }
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
async fn a_warm_runner_keeps_waiting_while_an_open_shell_holds_back_the_caches_check() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    docker.0.lock().unwrap().images = vec![records::record_image(10, 60)];
    let config = warm_config().replace("slots = 2", "slots = 2\nrecord_failures = true");
    let (mut engine, handle, _dir) = active(&github, &docker, &config).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);

    let size = docker::TtySize { cols: 80, rows: 24 };
    engine.open_shell(10, size).await.unwrap();
    // The check is due, but cannot run under the shell: stopping the runner for it would leave no runner, for nothing.
    engine.checked_at -= Duration::from_secs(3600);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert!(engine.trim.is_none(), "no check under an open shell");
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
    assert!(
        engine.count(engine.host_remembered()).plain <= 4608 << 20,
        "within MemoryMax"
    );
}

#[tokio::test]
async fn a_warm_runner_gone_without_a_job_after_waiting_holds_back_nothing() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;

    // Docker restarted under it an hour later: its container ended, and it took no job.
    for occupant in engine.occupants_mut() {
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
        engine.pool_hold("o/r", "app").unwrap().failures,
        1,
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
    engine.warm.testing_learned(Learned::default());
    for occupant in engine.occupants_mut() {
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
            handle.store.queued(
                "o/r",
                &ci_job(run * 10 + n, run, then + run * MINUTE),
                time::now_ms(),
            );
        }
    }
    let queued_at = time::now_ms() - 30 * MINUTE;
    handle
        .store
        .queued("o/r", &ci_job(50, 50, queued_at), time::now_ms());
    // A job of another workflow five minutes ago keeps o/r active.
    let lint = QueuedJob {
        workflow: Some("Lint".to_owned()),
        ..ci_job(60, 60, time::now_ms() - 5 * MINUTE)
    };
    handle.store.queued("o/r", &lint, time::now_ms());
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
        for occupant in engine.occupants_mut() {
            occupant.started_at -= Duration::from_secs(3600);
        }
        for container in &mut docker.0.lock().unwrap().containers {
            container.running = false;
        }
        engine.tick(Shutdown::No).await;
        assert_eq!(
            engine.pool_hold("o/r", "app").unwrap().held.is_some(),
            exits == 2,
            "held after the second in a row, not the first"
        );
    }
    assert_eq!(registered(&github.calls()).len(), 2, "no third");
}

/// A slot's worth of `count` jobs queued in `o/r`, each started, then all their runners connected and left: their
/// jobs were cancelled before they began.
async fn runners_connect_and_leave(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    github: &FakeGithub,
    docker: &FakeDocker,
    count: u64,
) {
    github.0.lock().unwrap().queued = (1..=count).map(job).collect();
    engine.tick(Shutdown::No).await;
    github.0.lock().unwrap().queued = Vec::new();
    let mut state = docker.0.lock().unwrap();
    state.runner_log = Some(CONNECTED_LOG.to_owned());
    for container in &mut state.containers {
        container.running = false;
    }
}

/// What a JIT runner logs when its job is gone before it acknowledges it: `Runner.cs` exits 0 and traces the reason.
const CONNECTED_LOG: &str = "\u{221a} Connected to GitHub\n\
    Current runner version: '2.338.1'\n\
    2026-11-07 13:29:01Z: Listening for Jobs\n\
    Runner listener exit with 0 return code, stop the service, no retry needed.\n\
    Exiting runner...\n";

#[tokio::test]
async fn runners_a_cancelled_run_took_with_it_hold_nothing_but_the_next_ones_leaving_do() {
    // Catches a cancelled run's second runner reading as a second fault, and a pool whose runners connect and leave
    // restarting one every few seconds for ever.
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();

    runners_connect_and_leave(&mut engine, &github, &docker, 2).await;
    assert_eq!(registered(&github.calls()).len(), 2, "both started");
    engine.tick(Shutdown::No).await;
    assert!(
        engine.pool_hold("o/r", "app").unwrap().held.is_none(),
        "both were up when the first left: one event"
    );

    runners_connect_and_leave(&mut engine, &github, &docker, 2).await;
    engine.tick(Shutdown::No).await;
    let held = engine.pool_hold("o/r", "app").unwrap().held;
    assert_eq!(
        held.map(|held| held.cause),
        Some(HoldCause::IdleExits),
        "these started after the first left, so it is no cancelled run: a fault"
    );
}

#[tokio::test]
async fn a_cancelled_run_starts_no_runner_for_its_jobs_from_the_listing_that_showed_them() {
    // Catches each cancelled job getting a second runner, which then waits out `IDLE_TIMEOUT` holding its reservation:
    // a pass starts runners from the last listing, which still shows the jobs, and no hold keeps it from them.
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();

    runners_connect_and_leave(&mut engine, &github, &docker, 2).await;
    engine.exits.any();
    engine.turnover().await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "no runner for a job the runner that left may have been given"
    );
    assert_eq!(engine.occupied().count(), 0, "both slots are free");

    // The next listing settles it: a job still queued gets its runner one poll late at most.
    github.0.lock().unwrap().queued = vec![job(1)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 3);
}

#[tokio::test]
async fn a_runner_that_never_connected_still_holds_its_pool_on_its_first_exit() {
    // Catches the rule for runners that connected reaching runners that could not.
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![job(1)];
    engine.tick(Shutdown::No).await;
    {
        let mut state = docker.0.lock().unwrap();
        state.runner_log =
            Some("Failed to create a session. The runner registration has been deleted".to_owned());
        state.containers[0].running = false;
    }
    engine.tick(Shutdown::No).await;
    let held = engine.pool_hold("o/r", "app").unwrap().held;
    assert_eq!(held.map(|held| held.cause), Some(HoldCause::NoJob));
}

#[tokio::test]
async fn a_warm_runner_that_waited_out_its_time_ends_the_failure_streak_before_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    // A failure days ago, and no job since to end its streak.
    engine
        .repositories
        .get_mut("o/r")
        .unwrap()
        .holds
        .entry("app".to_owned())
        .or_default()
        .failures = 1;

    for occupant in engine.occupants_mut() {
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
        engine.pool_hold("o/r", "app").unwrap().failures,
        0,
        "the next failure is announced as a new streak's first"
    );
}

#[tokio::test]
async fn a_warm_runner_whose_labels_image_or_limits_a_reload_changed_makes_way_for_one_that_has_them()
 {
    let gpu = warm_config().replace(
        "labels = [\"self-hosted\", \"app\"]",
        "labels = [\"self-hosted\", \"app\", \"gpu\"]",
    );
    let more_memory = warm_config().replace("memory = \"4G\"", "memory = \"6G\"");
    let other_image = warm_config().replace("image_stage = \"app\"", "image_stage = \"app-next\"");
    for config in [gpu, more_memory, other_image] {
        let github = FakeGithub::default();
        // Each runner gets an ID of its own: the old one is removed before the new ones register.
        github.0.lock().unwrap().strict = true;
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
    engine.make_reseed_due("app");
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert!(
        engine.no_reseeds(),
        "no reseed runs while the shell is open"
    );
}

#[tokio::test]
async fn a_warm_runner_keeps_waiting_for_a_due_reseed_whose_seed_would_not_fit() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);

    // The slice shrinks under the 4G seed container: stopping the warm runner would not make room for the reseed.
    engine.memory_max = Some(3 << 30);
    engine.make_reseed_due("app");
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    engine.publish();
    assert_eq!(slots(&handle), [Some(warm_in("o/r")), None]);
    assert!(engine.no_reseeds(), "the reseed waits for room");
}

#[tokio::test]
async fn a_warm_runner_on_the_old_image_waits_until_the_new_one_is_pulled() {
    const NEXT: &str = "ghcr.io/ayagmar/runnerd-runner:app-0.0.6";
    let on = |image: &str| {
        warm_config().replace("image_stage = \"app\"", &format!("image = \"{image}\""))
    };
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &on(PUBLISHED)).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let before = runner_ids(&handle, &mut engine);
    assert_eq!(before.len(), 1, "a warm runner waits");

    // A reload names the next release's image, which takes a while to pull: the old runner still serves meanwhile.
    {
        let mut state = docker.0.lock().unwrap();
        state.missing_images.insert(NEXT.to_owned());
        state.block_pulls = true;
    }
    engine.reload(Config::parse(&on(NEXT)).unwrap(), None, None);
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(
        runner_ids(&handle, &mut engine),
        before,
        "it waits on the old image"
    );

    // Once pulled, it makes way for one on the new image.
    docker.0.lock().unwrap().block_pulls = false;
    for _ in 0..100 {
        if docker
            .0
            .lock()
            .unwrap()
            .started_images
            .last()
            .map(String::as_str)
            == Some(NEXT)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(
        docker.0.lock().unwrap().started_images,
        [PUBLISHED, NEXT],
        "the old runner made way for one on the new image"
    );
    assert_eq!(
        runner_ids(&handle, &mut engine).len(),
        1,
        "one runner waits"
    );
}

#[tokio::test]
async fn a_requested_reseed_starts_beside_a_warm_runner_while_another_job_waits_for_the_host() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let config = format!(
        "{}{}",
        warm_config().replace("memory = \"4G\"", "memory = \"1G\""),
        r#"
[profiles.big]
image_stage = "big"
cpus = 2
memory = "2G"
slots = 1

[profiles.heavy]
image_stage = "heavy"
cpus = 2
memory = "8G"
slots = 1

[[repositories]]
name = "o/t"
profile = "big"
labels = ["self-hosted", "big"]
warm = 0

[[repositories]]
name = "o/h"
profile = "heavy"
labels = ["self-hosted", "heavy"]
warm = 0
"#
    );
    let (mut engine, handle, _dir) = active(&github, &docker, &config).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "o/r's warm runner starts"
    );

    // A 32G host, margin 2G (3G while short), whose slice holds o/r's 1G warm runner and which has 10.5G
    // available: the slice may hold 9.5G, 8.5G while short. Heavy's 8G seed fits beside the warm runner, so its
    // room is held; o/t's 2G job waits for the host, which keeps it short, as stopping the warm runner frees only
    // 1G of it.
    handle.store.host_memory(api::HostMemory {
        at: time::now_ms() / 1000,
        total: 32 << 30,
        available: 21 << 29,
        slice: 1 << 30,
        some: None,
        full: None,
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    });
    engine.request_reseed("heavy").unwrap();
    github.0.lock().unwrap().queued = vec![QueuedJob {
        labels: vec!["self-hosted".to_owned(), "big".to_owned()],
        ..job(5)
    }];
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    assert!(engine.reseeding("heavy"), "the seed starts");
    assert_eq!(registered(&github.calls()).len(), 1, "o/t's job waits");
}

/// `o/r`'s `CI` run 50's job, queued half an hour ago, runs in slot 1 on a runner a restart adopts.
async fn a_job_runs_since_half_an_hour(
    github: &FakeGithub,
    docker: &FakeDocker,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    let dir = TestDir::new("engine");
    running_job(github, docker, "o/r", 1, 50);
    let (engine, handle) = engine_with(github, docker, &dir, config);
    let queued_at = time::now_ms() - 30 * MINUTE;
    handle
        .store
        .queued("o/r", &ci_job(50, 50, queued_at), time::now_ms());
    handle.store.claimed(
        50,
        50,
        &identity("ci", ID_A).runner_name("app", 1, 1),
        queued_at + MINUTE,
    );
    handle.store.flush().await;
    (engine, handle, dir)
}

#[tokio::test]
async fn a_job_that_finishes_keeps_its_repository_active_for_the_next_push() {
    // Catches a repository going quiet while its long job runs and staying quiet after it: the owner's next push,
    // minutes after the job's end, would start cold.
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, _handle, _dir) =
        a_job_runs_since_half_an_hour(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()),
        Vec::<&String>::new(),
        "taken 29 minutes ago: quiet"
    );

    {
        let mut state = docker.0.lock().unwrap();
        state.runner_log =
            Some("Running job: build\nJob build completed with result: Succeeded".to_owned());
        state.containers[0].running = false;
    }
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "its end makes o/r active: a warm runner waits"
    );
}

#[tokio::test]
async fn an_adopted_idle_runner_of_a_repository_quiet_for_ten_minutes_goes_once_it_waited_out_its_time()
 {
    // Catches a restart keeping a quiet repository's warm runner: by a window left at an hour, or by an idle time
    // counted from the restart rather than from its container's start.
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    idle_runner(&github, &docker);
    docker
        .0
        .lock()
        .unwrap()
        .started
        .insert("container-1".to_owned(), time::now_ms() - 6 * MINUTE);
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &warm_config());
    handle.store.queued(
        "o/r",
        &ci_job(1, 1, time::now_ms() - 11 * MINUTE),
        time::now_ms(),
    );
    handle.store.flush().await;
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(slots(&handle), [None, None], "it went");
    assert_eq!(
        registered(&github.calls()),
        Vec::<&String>::new(),
        "and none came"
    );
}

/// `o/r` in `app` for jobs labelled `app` and, until the reload, `light`; after it, `light` jobs go to the `light`
/// pool, listed first. Four slots each.
fn moved_labels(light_pool: bool) -> String {
    let light = if light_pool {
        "[[repositories]]\nname = \"o/r\"\nprofile = \"light\"\nlabels = [\"self-hosted\", \"light\"]\n\n"
    } else {
        ""
    };
    format!(
        "{}\n[profiles.light]\nimage_stage = \"app\"\ncpus = 1\nmemory = \"1G\"\nslots = 4\n\n{light}\
         [[repositories]]\nname = \"o/r\"\nprofile = \"app\"\nlabels = [\"self-hosted\", \"app\", \"light\"]\n",
        warm_config()
            .replace("slots = 2", "slots = 4")
            .split("[[repositories]]")
            .next()
            .unwrap()
    )
}

#[tokio::test]
async fn a_reload_that_moves_labels_mid_run_learns_each_pools_bursts_again_and_keeps_its_activity()
{
    // Catches a reload keeping the old pools' bursts until the next read (the jvm-small split), and a re-read that
    // drops what the listings noted since the last one.
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &moved_labels(false));
    // Three runs of CI two days ago, each one `app` job and two `light` ones.
    let then = time::now_ms() - 2 * 24 * 60 * MINUTE;
    let light = |id, run, at| QueuedJob {
        labels: vec!["self-hosted".to_owned(), "light".to_owned()],
        ..ci_job(id, run, at)
    };
    for run in 1..=3 {
        let at = then + run * MINUTE;
        handle
            .store
            .queued("o/r", &ci_job(run * 10, run, at), time::now_ms());
        handle
            .store
            .queued("o/r", &light(run * 10 + 1, run, at), time::now_ms());
        handle
            .store
            .queued("o/r", &light(run * 10 + 2, run, at), time::now_ms());
    }
    handle.store.flush().await;
    engine.recover().await.unwrap();
    // Run 50's first job, a light one queued 20 minutes ago, is listed now: o/r is active from now.
    github.0.lock().unwrap().queued = vec![light(100, 50, time::now_ms() - 20 * MINUTE)];
    engine.tick(Shutdown::No).await;
    let app = Pool {
        repository: "o/r".to_owned(),
        profile: "app".to_owned(),
    };
    let light_pool = Pool {
        profile: "light".to_owned(),
        ..app.clone()
    };
    assert_eq!(
        engine.warm_targets()[&app],
        1 + 2,
        "one warm, and the run's two more of 3 jobs per run"
    );
    github.0.lock().unwrap().queued.clear();

    engine.reload(Config::parse(&moved_labels(true)).unwrap(), None, None);
    engine.tick(Shutdown::No).await;
    let targets = engine.warm_targets();
    assert_eq!(targets[&app], 1, "one warm; CI runs have one app job, seen");
    // The run's light job was seen under `app`: the light pool expects one more than it will get, at most a run's
    // burst, until the run stops counting.
    assert_eq!(
        targets[&light_pool],
        1 + 2,
        "one warm, and two light jobs per run"
    );
}

/// A step of a run of `o/r`'s `CI` workflow, which queues 1 job, then 2 more (its wave); 3 per run, as its history.
#[derive(Debug, Clone, Copy)]
enum RunStep {
    /// GitHub queues a new run's first job.
    Start,
    /// GitHub queues the wave of a run that has not queued it.
    Wave(usize),
    /// GitHub gives a queued job to an idle runner, which may be before runnerd lists it.
    Take(usize),
    /// A running job ends and its runner leaves.
    Finish(usize),
    /// The daemon restarts on the same history and Docker.
    Restart,
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum JobPhase {
    Queued,
    Running,
    Done,
}

#[tokio::test]
async fn restarts_in_the_middle_of_runs_start_no_more_than_each_runs_burst_less_its_open_jobs() {
    // Catches a restart counting a run's jobs again or as a new run: `Runs` comes back from the open jobs only, so a
    // run whose first job finished expects its whole burst less the jobs still open, never more (ADR 0107).
    const BURST: usize = 3;
    let cases = std::env::var("RUNNERD_WARM_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(12_u64);
    let only: Option<u64> = std::env::var("RUNNERD_WARM_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok());
    for seed in only.map_or(0..cases, |seed| seed..seed + 1) {
        let mut state = 0x5eed_u64.wrapping_add(seed);
        let mut below = |n: u64| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            usize::try_from((z ^ (z >> 31)) % n).unwrap()
        };
        let steps: Vec<RunStep> = (0..24)
            .map(|_| match below(12) {
                0 | 1 => RunStep::Start,
                2 | 3 => RunStep::Wave(below(4)),
                4..=6 => RunStep::Take(below(8)),
                7 | 8 => RunStep::Finish(below(8)),
                9 => RunStep::Restart,
                _ => RunStep::Tick,
            })
            .collect();
        if let Err(broken) = run_steps(&steps, BURST).await {
            panic!("seed {seed}: {broken}\n{steps:?}");
        }
    }
}

/// Runs `steps`, ticking after each, and checks the bound after every tick.
async fn run_steps(steps: &[RunStep], burst: usize) -> Result<(), String> {
    let github = FakeGithub::default();
    // Each runner gets an ID of its own, as runners come and go at random.
    github.0.lock().unwrap().strict = true;
    let docker = FakeDocker::default();
    docker.0.lock().unwrap().captures_wait = Some(github.clone());
    let dir = TestDir::new("engine");
    let config = warm_config().replace("slots = 2", "slots = 16");
    let (mut engine, mut handle) = engine_with(&github, &docker, &dir, &config);
    let then = time::now_ms() - 2 * 24 * 60 * MINUTE;
    for run in 1..=3 {
        for n in 0..3 {
            handle.store.queued(
                "o/r",
                &ci_job(run * 10 + n, run, then + run * MINUTE),
                time::now_ms(),
            );
        }
    }
    handle.store.flush().await;
    engine.recover().await.map_err(|error| error.to_string())?;
    // Each run's jobs, by ID: their phase.
    let mut runs: Vec<BTreeMap<u64, JobPhase>> = Vec::new();
    let mut waved: Vec<bool> = Vec::new();
    let mut noted: Vec<usize> = Vec::new();
    let mut next_job = 1000;
    for (index, step) in steps.iter().enumerate() {
        match *step {
            RunStep::Start => {
                let run = 100 + runs.len() as u64;
                github
                    .0
                    .lock()
                    .unwrap()
                    .queued
                    .push(ci_job(next_job, run, time::now_ms()));
                runs.push(BTreeMap::from([(next_job, JobPhase::Queued)]));
                noted.push(1);
                waved.push(false);
                next_job += 1;
            }
            RunStep::Wave(pick) => {
                let open: Vec<usize> = (0..runs.len()).filter(|run| !waved[*run]).collect();
                if let Some(&run) = open.get(pick % open.len().max(1)) {
                    for _ in 0..burst - 1 {
                        github.0.lock().unwrap().queued.push(ci_job(
                            next_job,
                            100 + run as u64,
                            time::now_ms(),
                        ));
                        runs[run].insert(next_job, JobPhase::Queued);
                        next_job += 1;
                    }
                    waved[run] = true;
                    noted[run] += burst - 1;
                }
            }
            RunStep::Take(pick) => take(&engine, &github, &docker, &handle, &mut runs, pick).await,
            RunStep::Finish(pick) => finish(&github, &docker, &mut runs, pick),
            RunStep::Restart => {
                engine.abort_captures();
                handle.store.flush().await;
                (engine, handle) = engine_with(&github, &docker, &dir, &config);
                engine.recover().await.map_err(|error| error.to_string())?;
                for (run, jobs) in runs.iter().enumerate() {
                    noted[run] = jobs.values().filter(|p| **p != JobPhase::Done).count();
                }
            }
            RunStep::Tick => {}
        }
        let before = registered(&github.calls()).len();
        engine.tick(Shutdown::No).await;
        let started = registered(&github.calls()).len() > before;
        let queued = github.0.lock().unwrap().queued.len();
        let expected: usize = noted
            .iter()
            .filter(|n| **n > 0)
            .map(|n| burst.saturating_sub(*n))
            .sum();
        let idle = engine
            .occupied()
            .filter(|(_, occupant)| !occupant.listed_busy() && occupant.named_job().is_none())
            .count();
        // One warm runner, as o/r is active, one per queued job, and per run its burst less its open jobs.
        if started && idle > 1 + queued + expected {
            return Err(format!(
                "after step {index} ({step:?}): {idle} idle runners, over 1 warm + {queued} queued + {expected} expected"
            ));
        }
    }
    Ok(())
}

/// GitHub gives a queued job to an idle runner; its capture names the job.
async fn take(
    engine: &Engine<FakeGithub, FakeDocker>,
    github: &FakeGithub,
    docker: &FakeDocker,
    handle: &Handle,
    runs: &mut [BTreeMap<u64, JobPhase>],
    pick: usize,
) {
    let idle: Vec<(String, String)> = engine
        .occupied()
        .filter(|(_, occupant)| !occupant.listed_busy() && occupant.named_job().is_none())
        .map(|(_, occupant)| (occupant.container.clone(), occupant.runner_name.clone()))
        .collect();
    let queued: Vec<(usize, u64)> = runs
        .iter()
        .enumerate()
        .flat_map(|(run, jobs)| {
            jobs.iter()
                .filter(|(_, phase)| **phase == JobPhase::Queued)
                .map(move |(id, _)| (run, *id))
        })
        .collect();
    let (Some((container, name)), Some(&(run, id))) =
        (idle.first(), queued.get(pick % queued.len().max(1)))
    else {
        return;
    };
    let run_id = 100 + run as u64;
    {
        let mut state = github.0.lock().unwrap();
        state.queued.retain(|job| job.id != id);
        for runner in &mut state.runners {
            if runner.name == *name {
                runner.busy = true;
            }
        }
        let mut running = details(id, "in_progress", None);
        running.run_id = run_id;
        running.workflow_name = Some("CI".to_owned());
        running.runner_name = Some(name.clone());
        // Queued when the run queued it, not on the fake's fixed date.
        running.created_at = None;
        state.jobs.insert(id, running);
    }
    docker
        .0
        .lock()
        .unwrap()
        .container_captures
        .insert(container.clone(), format!("@@job {id} {run_id}\n"));
    runs[run].insert(id, JobPhase::Running);
    for _ in 0..100 {
        handle.store.flush().await;
        if handle
            .store
            .job(id)
            .await
            .unwrap()
            .is_some_and(|job| job.summary.runner.is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A running job ends; GitHub forgets its runner, and its container stops.
fn finish(
    github: &FakeGithub,
    docker: &FakeDocker,
    runs: &mut [BTreeMap<u64, JobPhase>],
    pick: usize,
) {
    let running: Vec<(usize, u64)> = runs
        .iter()
        .enumerate()
        .flat_map(|(run, jobs)| {
            jobs.iter()
                .filter(|(_, phase)| **phase == JobPhase::Running)
                .map(move |(id, _)| (run, *id))
        })
        .collect();
    let Some(&(run, id)) = running.get(pick % running.len().max(1)) else {
        return;
    };
    let name = {
        let mut state = github.0.lock().unwrap();
        let name = state.jobs.get(&id).and_then(|job| job.runner_name.clone());
        let mut completed = details(id, "completed", Some("success"));
        completed.run_id = 100 + run as u64;
        completed.workflow_name = Some("CI".to_owned());
        completed.runner_name = name.clone();
        completed.created_at = None;
        state.jobs.insert(id, completed);
        state
            .runners
            .retain(|runner| Some(&runner.name) != name.as_ref());
        name
    };
    let mut state = docker.0.lock().unwrap();
    for container in &mut state.containers {
        if Some(&container.name) == name.as_ref() {
            container.running = false;
        }
    }
    runs[run].insert(id, JobPhase::Done);
}

#[tokio::test]
async fn a_run_seen_in_the_tick_idle_runners_wait_out_their_time_keeps_them_for_its_jobs() {
    // Catches expiry decided before the listing's run is noted: the quiet repository's idle runners would go in the
    // very tick a run that wants them shows up, and new ones start for it a tick later.
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    for slot in 1..=4 {
        let id = 6 + slot as u64;
        let name = identity("ci", ID_A).runner_name("app", slot, 1);
        let mut state = docker.0.lock().unwrap();
        state.containers.push(runner_container(slot, id, true));
        state.started.insert(
            format!("container-{slot}"),
            time::now_ms() - 2 * u64::try_from(IDLE_TIMEOUT.as_millis()).unwrap(),
        );
        drop(state);
        let mut state = github.0.lock().unwrap();
        state.runners.push(runner(id, &name, false));
        state.registered_on.insert(id, "o/r".to_owned());
    }
    let dir = TestDir::new("engine");
    let config = warm_config().replace("slots = 2", "slots = 6");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    // Three runs of CI two days ago, three jobs each: o/r is quiet now.
    let then = time::now_ms() - 2 * 24 * 60 * MINUTE;
    for run in 1..=3 {
        for n in 0..3 {
            handle.store.queued(
                "o/r",
                &ci_job(run * 10 + n, run, then + run * MINUTE),
                time::now_ms(),
            );
        }
    }
    handle.store.flush().await;
    engine.recover().await.unwrap();
    // A new run's first job comes in the tick the four runners have waited past IDLE_TIMEOUT.
    github.0.lock().unwrap().queued = vec![ci_job(100, 50, time::now_ms())];
    engine.tick(Shutdown::No).await;
    assert!(
        !github
            .calls()
            .iter()
            .any(|call| call.starts_with("delete ")),
        "one for the job, one warm and two for the run's other jobs stay: {:?}",
        github.calls()
    );
    assert_eq!(
        registered(&github.calls()),
        Vec::<&String>::new(),
        "none starts"
    );
}

/// Whether `o/r`'s `app` pool is held back, and how many runner failures were announced.
async fn held_and_announced(
    engine: &Engine<FakeGithub, FakeDocker>,
    handle: &Handle,
) -> (bool, usize) {
    handle.store.flush().await;
    let announced = handle
        .store
        .events_after(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|envelope| matches!(envelope.event, api::Event::RunnerFailed { .. }))
        .count();
    let held = engine
        .pool_hold("o/r", "app")
        .is_some_and(|hold| hold.held.is_some());
    (held, announced)
}

#[tokio::test]
async fn a_used_up_rate_limit_starts_no_warm_runner_and_holds_no_pool() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    let until = SystemTime::now() + Duration::from_mins(30);
    github.0.lock().unwrap().rate_limited = Some(until);
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 0);
    assert_eq!(held_and_announced(&engine, &handle).await, (false, 0));
    engine.publish();
    assert_eq!(
        handle.status.borrow().github_limited_until,
        Some(
            until
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        )
    );
}

#[tokio::test]
async fn a_registration_refused_for_the_rate_limit_holds_no_pool_and_starts_resume_once_it_resets()
{
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = active(&github, &docker, &warm_config()).await;
    engine.recover().await.unwrap();
    // The listings used up what was left.
    let until = SystemTime::now() + Duration::from_millis(300);
    github.0.lock().unwrap().registrations_limited = Some(until);
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1, "tried once");
    assert_eq!(held_and_announced(&engine, &handle).await, (false, 0));
    // The limit resets.
    github.0.lock().unwrap().registrations_limited = None;
    while SystemTime::now() <= until {
        std::thread::sleep(Duration::from_millis(20));
    }
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2, "started again");
    engine.publish();
    assert_eq!(handle.status.borrow().github_limited_until, None);
}
