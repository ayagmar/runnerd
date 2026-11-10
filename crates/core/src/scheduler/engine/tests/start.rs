//! A start in flight on the fakes: a command waits for it, and a daemon that goes in the middle of one leaves
//! nothing the next start does not clean up.

use super::*;

/// A queued job of `o/r` on a strict GitHub, and an engine that recovered.
async fn job_queued(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    {
        let mut state = github.0.lock().unwrap();
        state.strict = true;
        state.queued = vec![job(1)];
    }
    let (mut engine, handle) = engine_in(github, docker, dir);
    engine.recover().await.unwrap();
    (engine, handle)
}

/// Runs a tick until a runner's container start is held, then drops it, as a daemon that goes does.
async fn tick_until_the_start_is_held(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    docker: &FakeDocker,
) {
    tokio::select! {
        _ = engine.tick(Shutdown::No) => panic!("the tick ended before its start was held"),
        () = docker.until_start_held() => {}
    }
}

fn registrations(github: &FakeGithub) -> Vec<Runner> {
    github.0.lock().unwrap().runners.clone()
}

fn runner_containers(docker: &FakeDocker) -> Vec<String> {
    docker
        .0
        .lock()
        .unwrap()
        .containers
        .iter()
        .filter(|container| container.labels.contains_key("runnerd.job"))
        .map(|container| container.name.clone())
        .collect()
}

#[tokio::test]
async fn a_reload_sent_while_a_start_is_held_is_answered_after_it_and_leaves_its_runner_as_it_started()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, handle) = job_queued(&github, &docker, &dir).await;
    docker.hold_started();
    // `app` goes, and `o/r` asks for one more label in `big`.
    let config = CONFIG
        .replace("[profiles.app]", "[profiles.big]")
        .replace("profile = \"app\"", "profile = \"big\"")
        .replace(
            "[\"self-hosted\", \"app\"]",
            "[\"self-hosted\", \"app\", \"big\"]",
        );
    let control = handle.control.clone();
    let reload = || {
        tokio::spawn(async move {
            control
                .replace_config(
                    Config::parse(&config).unwrap(),
                    None,
                    None,
                    Commit::new(|| Ok(None)),
                    RunningSlots::Drain,
                    Author::Owner,
                )
                .await
        })
    };
    let ((), (replaced, early)) = tokio::join!(
        async {
            engine.tick(Shutdown::No).await;
        },
        async {
            docker.until_start_held().await;
            let replaced = reload();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let early = replaced.is_finished();
            docker.release_started();
            (replaced, early)
        }
    );
    assert!(!early, "the reload waits for the start");
    while !replaced.is_finished() {
        engine
            .wait_for_tick(
                Shutdown::No,
                tokio::time::Instant::now() + Duration::from_secs(30),
            )
            .await;
    }
    assert_eq!(replaced.await.unwrap(), Ok(Vec::new()));
    assert!(engine.is_retired("app"), "app goes once its runner does");
    let held: Vec<(String, Option<Vec<String>>)> = engine
        .occupied()
        .map(|(slot, occupant)| (slot.profile, occupant.labels.clone()))
        .collect();
    assert_eq!(
        held,
        [(
            "app".to_owned(),
            Some(vec!["self-hosted".to_owned(), "app".to_owned()])
        )],
        "the runner keeps its slot and the labels it registered with"
    );
}

#[tokio::test]
async fn a_registration_a_daemon_left_without_a_container_is_deleted_by_the_next_start() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle) = job_queued(&github, &docker, &dir).await;
    docker.hold_starts();
    tick_until_the_start_is_held(&mut engine, &docker).await;
    drop(engine);
    docker.release_starts();
    let left = registrations(&github);
    assert_eq!(left.len(), 1, "the start registered its runner: {left:?}");
    assert_eq!(runner_containers(&docker), Vec::<String>::new());

    let (mut engine, _handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let registered = registrations(&github);
    assert!(
        registered.iter().all(|runner| runner.id != left[0].id),
        "the registration with no container is deleted: {:?}",
        github.calls()
    );
    assert_eq!(
        registered
            .iter()
            .map(|runner| runner.name.clone())
            .collect::<Vec<_>>(),
        runner_containers(&docker),
        "the job gets a runner of its own"
    );
}

#[tokio::test]
async fn a_container_a_daemon_left_in_no_slot_is_adopted_by_the_next_start() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle) = job_queued(&github, &docker, &dir).await;
    docker.hold_started();
    tick_until_the_start_is_held(&mut engine, &docker).await;
    drop(engine);
    docker.release_started();
    let started = runner_containers(&docker);
    assert_eq!(
        started.len(),
        1,
        "the start made its container: {started:?}"
    );

    let (mut engine, _handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let held: Vec<String> = engine
        .occupied()
        .map(|(_, occupant)| occupant.runner_name.clone())
        .collect();
    assert_eq!(held, started, "recovery adopts it into a slot");
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "the adopted runner is the job's"
    );
    assert_eq!(runner_containers(&docker), started);
}
