//! Stopping on the fakes (ADR 0091): a signal leaves every runner and the mirror to the next start, which takes them
//! over; a stop ends when Docker is gone or fails midway, and within `STOP_DEADLINE` when Docker hangs; a second
//! signal kills running jobs; a stop waits for a record being made but not for a reseed or a check of the caches.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::scheduler::engine::STOP_DEADLINE;

/// Job 1 runs in slot 1, an idle runner started just now waits in slot 2, and the mirror is up.
fn busy_and_idle(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
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
    let config = CONFIG.replace("mirror = { enabled = false", "mirror = { enabled = true");
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
async fn a_signal_leaves_runners_and_the_mirror_and_the_next_start_takes_them_over() {
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
    let before = container_names(&docker);
    let (github_calls, docker_calls) = (github.calls().len(), docker.calls().len());

    handle.control.shutdown(Shutdown::Detach);
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("a signal stops at once")
        .unwrap();
    assert_eq!(container_names(&docker), before, "every container stays");
    let deleted: Vec<String> = github.calls()[github_calls..]
        .iter()
        .filter(|call| call.starts_with("delete "))
        .cloned()
        .collect();
    assert_eq!(deleted, Vec::<String>::new(), "no runner is deregistered");
    let removed: Vec<String> = docker.calls()[docker_calls..]
        .iter()
        .filter(|call| call.starts_with("remove "))
        .cloned()
        .collect();
    assert_eq!(removed, Vec::<String>::new(), "nothing is removed");

    let (slots, handle) = next_start(&github, &docker, &dir).await;
    let name = |slot| Some(identity("ci", ID_A).runner_name("app", slot, 1));
    assert_eq!(
        slots,
        [name(1), name(2)],
        "each runner is adopted in its slot"
    );
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
    handle.store.flush().await;
    let job = handle.store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Running);
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
            // It answers inspects, so the idle runner is deregistered, then fails its removal.
            docker.0.lock().unwrap().fail_removes = u32::MAX;
        }

        // `runnerd stop`, which would wait for job 1 if Docker answered.
        handle.control.shutdown(Shutdown::Graceful);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap_or_else(|_| panic!("the stop never ended (Docker gone: {gone})"))
            .unwrap();
        let calls = github.calls();
        for id in [7, 8] {
            let deletes = calls
                .iter()
                .filter(|call| **call == format!("delete {id}"))
                .count();
            assert!(deletes <= 1, "runner {id} deleted {deletes} times");
        }

        docker.set_outage(Outage::None);
        docker.0.lock().unwrap().fail_removes = 0;
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
