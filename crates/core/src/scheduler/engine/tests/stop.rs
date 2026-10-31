//! Stopping on the fakes (ADR 0091): a signal removes idle runners and leaves busy ones and the mirror to the next
//! start, which takes them over; a stop ends when Docker is gone or fails midway, and within `STOP_DEADLINE` when Docker hangs; a second
//! signal kills running jobs; a stop waits for a record being made but not for a reseed or a check of the caches.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::api::Stopping;
use crate::scheduler::engine::STOP_DEADLINE;

/// Job 1 runs in slot 1, an idle runner started just now waits in slot 2, and the mirror is up.
fn busy_and_idle(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    busy_and_idle_with(github, docker, dir, CONFIG)
}

/// `busy_and_idle` on `config`, with the mirror enabled.
fn busy_and_idle_with(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    running_runner(github, docker, "@@job 1 1\n");
    let idle = identity("ci", ID_A).runner_name("app", 2, 1);
    {
        let mut state = github.0.lock().unwrap();
        state.jobs.insert(1, details(1, "in_progress", None));
        state.runners.push(runner(8, &idle, false));
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.containers.push(runner_container(2, 8, true));
        // Its log capture finds no job.
        state
            .container_captures
            .insert("container-2".to_owned(), String::new());
        state
            .started
            .insert("container-2".to_owned(), crate::time::now_ms());
        let mirror = Names::new("runnerd").mirror();
        state.containers.push(Container {
            id: mirror.clone(),
            name: mirror,
            running: true,
            labels: HashMap::from([("runnerd.mirror".to_owned(), String::new())]),
        });
    }
    let config = config.replace("mirror = { enabled = false", "mirror = { enabled = true");
    engine_with(github, docker, dir, &config)
}

fn container_names(docker: &FakeDocker) -> BTreeSet<String> {
    docker
        .0
        .lock()
        .unwrap()
        .containers
        .iter()
        .map(|container| container.name.clone())
        .collect()
}

/// The next start on the same history, Docker and GitHub: recovers and ticks once. Returns the runner in each slot.
async fn next_start(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Vec<Option<String>>, Handle) {
    let config = CONFIG.replace("mirror = { enabled = false", "mirror = { enabled = true");
    let (mut engine, handle) = engine_with(github, docker, dir, &config);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    let slots = handle.status.borrow().profiles[0]
        .slots
        .iter()
        .map(|slot| slot.runner.as_ref().map(|runner| runner.name.clone()))
        .collect();
    (slots, handle)
}

#[tokio::test]
async fn a_signal_removes_idle_runners_and_leaves_busy_ones_and_the_mirror_to_the_next_start() {
    // The idle runner may take a job right before the signal, which GitHub knows before runnerd does.
    for took_job in [false, true] {
        let (github, docker, dir) = (
            FakeGithub::default(),
            FakeDocker::default(),
            TestDir::new("stop"),
        );
        let (engine, handle) = busy_and_idle(&github, &docker, &dir);
        let run = tokio::spawn(engine.run());
        until_state(&handle, 1, JobState::Running).await;
        github
            .until_calls(
                |calls| calls.iter().any(|call| call.starts_with("runners ")),
                "asked for its runners",
            )
            .await;
        if took_job {
            github.0.lock().unwrap().busy_deletes = 1;
        }
        let (github_calls, docker_calls) = (github.calls().len(), docker.calls().len());

        handle.control.shutdown(Shutdown::Detach);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("a signal stops at once")
            .unwrap();
        let name = |slot| identity("ci", ID_A).runner_name("app", slot, 1);
        let mut kept = BTreeSet::from([name(1), Names::new("runnerd").mirror()]);
        if took_job {
            kept.insert(name(2));
        }
        assert_eq!(container_names(&docker), kept, "took a job: {took_job}");
        let deleted: Vec<String> = github.calls()[github_calls..]
            .iter()
            .filter(|call| call.starts_with("delete "))
            .cloned()
            .collect();
        assert_eq!(
            deleted,
            ["delete 8"],
            "only the idle runner is deregistered"
        );
        let removed: Vec<String> = docker.calls()[docker_calls..]
            .iter()
            .filter(|call| call.starts_with("remove "))
            .cloned()
            .collect();
        let expected: &[&str] = if took_job {
            &[]
        } else {
            &["remove container-2"]
        };
        assert_eq!(removed, expected, "took a job: {took_job}");

        let (slots, handle) = next_start(&github, &docker, &dir).await;
        assert_eq!(
            slots,
            [Some(name(1)), took_job.then(|| name(2))],
            "the next start adopts what runs (took a job: {took_job})"
        );
        assert_eq!(registered(&github.calls()), Vec::<&String>::new());
        handle.store.flush().await;
        let job = handle.store.job(1).await.unwrap().unwrap();
        assert_eq!(job.summary.state, JobState::Running);
    }
}

#[tokio::test]
async fn a_stop_ends_when_docker_is_gone_or_fails_midway_and_the_next_start_takes_over() {
    for gone in [true, false] {
        let (github, docker, dir) = (
            FakeGithub::default(),
            FakeDocker::default(),
            TestDir::new("stop"),
        );
        let (engine, handle) = busy_and_idle(&github, &docker, &dir);
        if gone {
            // Only job 1's runner, which no removal fails for: the stop must notice the failed inspects.
            docker
                .0
                .lock()
                .unwrap()
                .containers
                .retain(|container| container.id != "container-2");
            github
                .0
                .lock()
                .unwrap()
                .runners
                .retain(|runner| runner.id != 8);
        }
        let run = tokio::spawn(engine.run());
        until_state(&handle, 1, JobState::Running).await;
        github
            .until_calls(
                |calls| calls.iter().any(|call| call.starts_with("runners ")),
                "asked for its runners",
            )
            .await;
        if gone {
            docker.set_outage(Outage::Refused);
        } else {
            // It answers inspects, so the idle runner is deregistered, then goes before its removal.
            docker.0.lock().unwrap().remove_outage = Outage::Refused;
        }

        // `runnerd stop`, which would wait for job 1 if Docker answered.
        handle.control.shutdown(Shutdown::Graceful);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap_or_else(|_| panic!("the stop never ended (Docker gone: {gone})"))
            .unwrap();
        assert_eq!(
            handle.shutdown(Shutdown::Graceful),
            Some(Stopping::LeavesJobs),
            "a second `runnerd stop` says what the first does once Docker stopped answering (Docker gone: {gone})"
        );
        assert_eq!(handle.shutdown(Shutdown::Force), Some(Stopping::KillsJobs));
        let calls = github.calls();
        for id in [7, 8] {
            let deletes = calls
                .iter()
                .filter(|call| **call == format!("delete {id}"))
                .count();
            assert!(deletes <= 1, "runner {id} deleted {deletes} times");
        }

        docker.set_outage(Outage::None);
        docker.0.lock().unwrap().remove_outage = Outage::None;
        let (slots, handle) = next_start(&github, &docker, &dir).await;
        assert_eq!(
            slots[0],
            Some(identity("ci", ID_A).runner_name("app", 1, 1)),
            "job 1's runner is adopted (Docker gone: {gone})"
        );
        assert_eq!(registered(&github.calls()), Vec::<&String>::new());
        handle.store.flush().await;
        let job = handle.store.job(1).await.unwrap().unwrap();
        assert_eq!(job.summary.state, JobState::Running);
    }
}

#[tokio::test]
async fn runnerd_stop_tries_again_a_removal_docker_refused_and_leaves_nothing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let (mut engine, handle) = busy_and_idle(&github, &docker, &dir);
    // Job 1 ended: both runners are idle, and the job's runner exits.
    github
        .0
        .lock()
        .unwrap()
        .runners
        .iter_mut()
        .for_each(|runner| runner.busy = false);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    docker.0.lock().unwrap().containers[0].running = false;
    // Docker answers the first removal with an error, as when it is busy.
    docker.0.lock().unwrap().fail_removes = 1;
    handle.control.shutdown(Shutdown::Graceful);
    tokio::time::timeout(Duration::from_secs(30), engine.schedule())
        .await
        .expect("the stop ends once Docker removes them");
    assert_eq!(
        container_names(&docker),
        BTreeSet::new(),
        "both runners and the mirror are removed"
    );
}

#[tokio::test]
async fn a_stop_ends_within_its_deadline_while_docker_hangs_and_keeps_the_history() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let (engine, handle) = busy_and_idle(&github, &docker, &dir);
    let run = tokio::spawn(engine.run());
    until_state(&handle, 1, JobState::Running).await;
    tokio::time::pause();
    docker.set_outage(Outage::Hung);
    let asked = tokio::time::Instant::now();
    handle.control.shutdown(Shutdown::Force);
    // systemd kills a unit that has not stopped 90s after it asked.
    tokio::time::timeout(Duration::from_secs(89), run)
        .await
        .expect("the stop outlasted systemd's timeout")
        .unwrap();
    assert!(asked.elapsed() >= STOP_DEADLINE, "it gave Docker its time");
    // What the daemon does once the engine returns.
    handle.store.close();
    handle.store.flush().await;
    let reopened = Store::open(&dir.0.join("history.db")).unwrap();
    let job = reopened.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Running);
}

#[tokio::test]
async fn a_second_signal_during_a_stop_kills_running_jobs() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let (engine, handle) = busy_and_idle(&github, &docker, &dir);
    let run = tokio::spawn(engine.run());
    until_state(&handle, 1, JobState::Running).await;
    // The signals come while a tick waits for GitHub.
    github.hold();
    handle.control.wake.notify_one();
    github.until_waiting(1).await;
    handle.control.shutdown(Shutdown::Detach);
    handle.control.shutdown(Shutdown::Force);
    github.release();
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        container_names(&docker),
        BTreeSet::new(),
        "both runners and the mirror are removed"
    );
    let calls = github.calls();
    assert!(
        calls.contains(&"delete 7".to_owned()),
        "job 1's runner is deregistered: {calls:?}"
    );
}

#[tokio::test]
async fn a_second_signal_while_a_signal_removes_idle_runners_kills_running_jobs() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let (engine, handle) = busy_and_idle(&github, &docker, &dir);
    let run = tokio::spawn(engine.run());
    until_state(&handle, 1, JobState::Running).await;
    github
        .until_calls(
            |calls| calls.iter().any(|call| call.starts_with("runners ")),
            "asked for its runners",
        )
        .await;
    // The second signal comes while GitHub has not answered the idle runner's deregistration.
    github.hold_deletes();
    handle.control.shutdown(Shutdown::Detach);
    github.until_waiting(1).await;
    handle.control.shutdown(Shutdown::Force);
    github.release_deletes();
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        container_names(&docker),
        BTreeSet::new(),
        "job 1's runner and the mirror are removed too"
    );
}

#[tokio::test]
async fn a_stop_leaves_a_reseed_and_a_check_of_the_caches_to_the_next_start() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let seed = Names::new("runnerd").seed("app");
    {
        let mut state = docker.0.lock().unwrap();
        // The seed container runs until the test ends, and measuring the volumes never finishes.
        state.containers.push(Container {
            id: "seed".to_owned(),
            name: seed.clone(),
            running: true,
            labels: HashMap::from([
                ("runnerd.seed".to_owned(), String::new()),
                ("runnerd.profile".to_owned(), "app".to_owned()),
            ]),
        });
        state.volume_sizes_held.send_replace(true);
    }
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert!(engine.trim.is_some(), "the caches are being checked");
    assert!(!engine.reseeds.is_empty(), "app is reseeding");

    handle.control.shutdown(Shutdown::Graceful);
    tokio::time::timeout(Duration::from_secs(10), engine.schedule())
        .await
        .expect("the stop waits for neither");
    assert!(container_names(&docker).contains(&seed), "the seed stays");

    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert!(
        handle.status.borrow().profiles[0].reseeding,
        "the next start waits for the reseed"
    );
    assert!(engine.trim.is_some(), "the next start checks the caches");
}

#[tokio::test]
async fn a_reload_asked_for_while_a_signal_stops_the_daemon_is_answered_as_it_went() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let (engine, handle) = engine_in(&github, &docker, &dir);
    let run = tokio::spawn(engine.run());
    github
        .until_calls(
            |calls| calls.iter().any(|call| call.starts_with("runners ")),
            "asked for its runners",
        )
        .await;
    github.hold();
    handle.control.wake.notify_one();
    github.until_waiting(1).await;
    handle.control.shutdown(Shutdown::Detach);
    let written = Arc::new(AtomicBool::new(false));
    let commit = {
        let written = written.clone();
        Commit::new(move || {
            written.store(true, Ordering::SeqCst);
            Ok(None)
        })
    };
    let control = handle.control.clone();
    let reload = tokio::spawn(async move {
        control
            .replace_config(
                Config::parse(&CONFIG.replace("slots = 2", "slots = 1")).unwrap(),
                None,
                None,
                commit,
                RunningSlots::Drain,
                Author::Owner,
            )
            .await
    });
    github.release();
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(10), reload)
        .await
        .expect("the reload is answered")
        .unwrap();
    match answer {
        Ok(_) => assert!(written.load(Ordering::SeqCst), "applied, so written"),
        Err(ControlError::Stopped) => {
            assert!(!written.load(Ordering::SeqCst), "refused, so not written");
        }
        Err(other) => panic!("the reload failed: {other:?}"),
    }
}

#[tokio::test]
async fn the_first_tick_after_an_install_replaces_idle_runners_of_the_old_release() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let (mut engine, _handle) = busy_and_idle(&github, &docker, &dir);
    // Both runners were started by an earlier release, whose container spec may differ.
    for container in &mut docker.0.lock().unwrap().containers {
        container
            .labels
            .insert(super::super::runners::SPEC.to_owned(), "0.0.1".to_owned());
    }
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let calls = docker.calls();
    assert!(
        calls.contains(&"remove container-2".to_owned()),
        "the idle runner goes: {calls:?}"
    );
    assert!(
        !calls.contains(&"remove container-1".to_owned()),
        "job 1's runner keeps running: {calls:?}"
    );
}

/// Job 1 runs in slot 1 and job 2 in slot 2, on a profile that keeps failed jobs' runners.
fn two_jobs_recording(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    running_runner(github, docker, "@@job 1 1\n");
    let second = identity("ci", ID_A).runner_name("app", 2, 1);
    {
        let mut state = github.0.lock().unwrap();
        state.jobs.insert(1, details(1, "in_progress", None));
        state.jobs.insert(2, details(2, "in_progress", None));
        state.runners.push(runner(8, &second, true));
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.containers.push(runner_container(2, 8, true));
        state
            .container_captures
            .insert("container-2".to_owned(), "@@job 2 2\n".to_owned());
    }
    engine_with(github, docker, dir, &records::recording())
}

/// Waits until the fake Docker was asked for `call`.
async fn until_docker_call(docker: &FakeDocker, call: &str) {
    for _ in 0..500 {
        if docker.calls().iter().any(|made| made == call) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Docker was never asked to {call}: {:?}", docker.calls());
}

#[test]
fn runnerd_stop_that_docker_turns_into_a_detach_ends_within_the_deadline_and_the_next_start_keeps_the_record()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    // The daemon's own runtime, gone with the process once `run` returns: so is the commit it left waiting.
    let daemon = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    daemon.block_on(async {
        let (engine, handle) = two_jobs_recording(&github, &docker, &dir);
        let run = tokio::spawn(engine.run());
        until_state(&handle, 2, JobState::Running).await;
        // Job 2 fails, and its record's commit waits for Docker.
        {
            let mut state = docker.0.lock().unwrap();
            state.commits_held.send_replace(true);
            state
                .container_logs
                .insert("container-2".to_owned(), records::FAILED.to_owned());
            state.containers[1].running = false;
        }
        github
            .0
            .lock()
            .unwrap()
            .jobs
            .insert(2, details(2, "completed", Some("failure")));
        handle.control.wake.notify_one();
        until_docker_call(&docker, "commit container-2 runnerd-record:2").await;

        // `runnerd stop`, while Docker no longer answers the inspect of job 1's runner.
        tokio::time::pause();
        docker.set_outage(Outage::Refused);
        let asked = tokio::time::Instant::now();
        handle.control.shutdown(Shutdown::Graceful);
        tokio::time::timeout(STOP_DEADLINE + Duration::from_secs(10), run)
            .await
            .expect("the detach Docker turned the stop into has the deadline of any detach")
            .unwrap();
        assert!(
            asked.elapsed() >= STOP_DEADLINE,
            "it gave the record its time"
        );
        handle.store.close();
        handle.store.flush().await;
    });
    drop(daemon);

    let next = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    next.block_on(async {
        {
            let mut state = docker.0.lock().unwrap();
            state.outage = Outage::None;
            state.commits_held.send_replace(false);
        }
        let (mut engine, handle) = engine_with(&github, &docker, &dir, &records::recording());
        engine.recover().await.unwrap();
        records::until_recorded(&mut engine).await;
        let note = records::note(&handle, 2).await;
        assert!(
            note.is_some_and(|note| note.created_at.is_some()),
            "job 2's runner becomes its record at the next start"
        );
        engine.publish();
        let runner = handle.status.borrow().profiles[0].slots[0]
            .runner
            .as_ref()
            .map(|runner| runner.name.clone());
        assert_eq!(
            runner,
            Some(identity("ci", ID_A).runner_name("app", 1, 1)),
            "job 1's runner is adopted"
        );
    });
}

#[tokio::test]
async fn a_failed_job_an_idle_runner_took_while_detached_becomes_its_record_at_the_next_start() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("stop"),
    );
    let config = records::recording();
    let (engine, handle) = busy_and_idle_with(&github, &docker, &dir, &config);
    let run = tokio::spawn(engine.run());
    until_state(&handle, 1, JobState::Running).await;
    github
        .until_calls(
            |calls| calls.iter().any(|call| call.starts_with("runners ")),
            "asked for its runners",
        )
        .await;
    // The idle runner takes job 5 right before the signal, so GitHub keeps it, and runnerd never sees the job.
    github.0.lock().unwrap().busy_deletes = 1;
    handle.control.shutdown(Shutdown::Detach);
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    drop(handle);

    // Job 5 fails before the next start, and its runner exits; only its worker log names the job.
    github
        .0
        .lock()
        .unwrap()
        .runners
        .retain(|runner| runner.id != 8);
    {
        let mut state = docker.0.lock().unwrap();
        let idle = state
            .containers
            .iter_mut()
            .find(|container| container.id == "container-2")
            .unwrap();
        idle.running = false;
        state
            .container_logs
            .insert("container-2".to_owned(), records::FAILED.to_owned());
        state
            .diag
            .insert("container-2".to_owned(), docker::tests::diag_archive(5));
        // Docker is too busy to read it at first, then the read breaks off before the worker log.
        state.fail_files = 1;
        state.cut_files = 1;
    }
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    let left = identity("ci", ID_A).runner_name("app", 2, 1);
    assert!(
        container_names(&docker).contains(&left),
        "the runner Docker could not read yet stays"
    );
    engine.tick(Shutdown::No).await;
    assert!(
        container_names(&docker).contains(&left),
        "the runner whose worker log was not read stays"
    );
    records::until_recorded(&mut engine).await;
    let kept: Vec<u64> = handle
        .records
        .borrow()
        .records
        .iter()
        .map(|record| record.job)
        .collect();
    assert_eq!(kept, [5], "job 5's runner becomes its record");
    let calls = docker.calls();
    let commit = calls
        .iter()
        .position(|call| call == "commit container-2 runnerd-record:5");
    let remove = calls.iter().position(|call| call == "remove container-2");
    assert!(
        commit.is_some() && commit < remove,
        "the stopped runner is committed, then goes: {calls:?}"
    );
}
