//! A start in flight on the fakes: a command waits for it, and a daemon that goes in the middle of one leaves
//! nothing the next start does not clean up.

use super::build_cache::engine_with_cache;
use futures_util::FutureExt as _;

use super::*;
use crate::admission::{host, pooled};
use crate::buildcache::BuildCache;
use crate::scheduler::engine::start::Starting;
use crate::scheduler::{Decision, SlotRef};

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

const G: u64 = 1 << 30;

/// Slot 1 of `app`.
fn slot_1() -> SlotRef {
    SlotRef {
        profile: "app".to_owned(),
        index: 0,
    }
}

/// Whether the slice's sample is recent, as measured admission asks.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Sampled {
    Fresh,
    Stale,
}

/// What the budgets read of the slots: the aggregate count, the CPU in use, and what the host's margin goes by.
#[derive(Debug, PartialEq)]
struct Counted {
    count: pooled::Count,
    cpu: u64,
    host: host::Remembered,
}

fn counted(engine: &Engine<FakeGithub, FakeDocker>) -> Counted {
    let host = engine.host_remembered();
    Counted {
        count: engine.count(host),
        cpu: engine.cpu_in_use(),
        host,
    }
}

#[tokio::test]
async fn a_start_in_flight_counts_as_the_runner_it_becomes() {
    let measured = CONFIG.replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\nadmission = \"measured\"",
    );
    for (policy, sampled) in [
        (api::AdmissionPolicy::Caps, Sampled::Fresh),
        (api::AdmissionPolicy::Measured, Sampled::Fresh),
        (api::AdmissionPolicy::Measured, Sampled::Stale),
    ] {
        let (github, docker, dir) = (
            FakeGithub::default(),
            FakeDocker::default(),
            TestDir::new("start"),
        );
        let config = match policy {
            api::AdmissionPolicy::Caps => CONFIG,
            api::AdmissionPolicy::Measured => &measured,
        };
        let (mut engine, handle) = engine_with(&github, &docker, &dir, config);
        engine.memory_max = Some(16 * G);
        if sampled == Sampled::Fresh {
            handle.store.host_sample(api::HostSample {
                at: crate::time::now_ms() / 1000,
                cpu: 0,
                memory: 2 * G,
            });
        }
        let admission = api::Admission {
            policy,
            reserved: G,
            basis: api::Basis::Job {
                runs: 10,
                peak: api::JobPeak::P95,
            },
            free: None,
            slice: None,
            cpu: Some(api::CpuAdmission {
                reserved: 500,
                basis: api::Basis::Profile { runs: 10 },
                free: 4000,
                waited: None,
            }),
        };
        let starting = Starting::new("o/r", "runnerd-ci-app-1-1", admission, 4 * G, 2000);
        let case = format!("{policy:?}, {sampled:?} slice sample");
        let empty = counted(&engine);

        engine.slot_mut(&slot_1()).launch(starting.clone());
        let in_flight = counted(&engine);
        assert!(
            engine.slot_mut(&slot_1()).end_start().is_some(),
            "{case}: the start ends"
        );
        engine
            .slot_mut(&slot_1())
            .place(starting.into_occupant(1, "container-1".to_owned()));
        let placed = counted(&engine);

        assert_eq!(in_flight, placed, "{case}: a start counts as its runner");
        // Its cap, unless measured admission has a recent sample to go by its reservation.
        let memory = match (policy, sampled) {
            (api::AdmissionPolicy::Measured, Sampled::Fresh) => G,
            _ => 4 * G,
        };
        assert_eq!(in_flight.count.plain, memory, "{case}");
        let cpu = match policy {
            api::AdmissionPolicy::Caps => 2000,
            api::AdmissionPolicy::Measured => 500,
        };
        assert_eq!(in_flight.cpu, cpu, "{case}");
        assert_ne!(
            in_flight.host, empty.host,
            "{case}: the host's margin sees a runner in the slice"
        );
    }
}

/// `job_queued`, with a build cache.
async fn job_queued_with_cache(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle, BuildCache) {
    {
        let mut state = github.0.lock().unwrap();
        state.strict = true;
        state.queued = vec![job(1)];
    }
    let (mut engine, handle, cache) = engine_with_cache(github, docker, dir, CONFIG);
    engine.recover().await.unwrap();
    (engine, handle, cache)
}

/// The runner the first registration named.
fn first_registered(github: &FakeGithub) -> String {
    let calls = github.calls();
    let registered = registered(&calls);
    registered
        .first()
        .and_then(|call| call.strip_prefix("register "))
        .unwrap()
        .to_owned()
}

/// Whether `runner`'s build cache socket is open.
fn cache_open(cache: &BuildCache, runner: &str) -> bool {
    cache.undecided(runner)
}

fn held_for(engine: &Engine<FakeGithub, FakeDocker>) -> Option<api::HoldCause> {
    engine
        .pool_hold("o/r", "app")
        .and_then(|hold| hold.held)
        .map(|held| held.cause)
}

#[tokio::test]
async fn a_registration_github_fails_frees_the_slot_closes_the_build_cache_and_holds_the_pool() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle, cache) = job_queued_with_cache(&github, &docker, &dir).await;
    github.0.lock().unwrap().fail_registrations = 1;
    engine.tick(Shutdown::No).await;
    let runner = first_registered(&github);
    assert!(!cache_open(&cache, &runner), "its build cache is closed");
    assert_eq!(engine.occupied().count(), 0, "its slot is free");
    assert!(!engine.any_starting(), "no start is left in flight");
    assert_eq!(held_for(&engine), Some(api::HoldCause::Register));
}

#[tokio::test]
async fn a_registration_refused_for_the_rate_limit_closes_the_build_cache_and_holds_no_pool() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle, cache) = job_queued_with_cache(&github, &docker, &dir).await;
    github.0.lock().unwrap().limit_registrations = 1;
    engine.tick(Shutdown::No).await;
    let runner = first_registered(&github);
    assert!(!cache_open(&cache, &runner), "its build cache is closed");
    assert_eq!(engine.occupied().count(), 0, "its slot is free");
    assert!(!engine.any_starting(), "no start is left in flight");
    assert!(
        engine.github_limit().is_some(),
        "no runner starts until the limit resets"
    );
    assert_eq!(held_for(&engine), None, "not the pool's fault");
}

#[tokio::test]
async fn a_stop_while_github_registers_deletes_the_registration_and_closes_the_build_cache() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, handle, cache) = job_queued_with_cache(&github, &docker, &dir).await;
    github.hold_registrations();
    let stops = async {
        github.until_waiting(1).await;
        handle.control.shutdown(Shutdown::Detach);
        github.release_registrations();
    };
    tokio::join!(engine.tick(Shutdown::No), stops);
    let runner = first_registered(&github);
    assert!(!cache_open(&cache, &runner), "its build cache is closed");
    assert_eq!(
        registrations(&github),
        Vec::new(),
        "its registration is deleted"
    );
    assert_eq!(runner_containers(&docker), Vec::<String>::new());
    assert!(!engine.any_starting(), "no start is left in flight");
}

#[tokio::test]
async fn a_start_docker_fails_without_a_container_deletes_the_registration_and_closes_the_build_cache()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle, cache) = job_queued_with_cache(&github, &docker, &dir).await;
    docker.0.lock().unwrap().fail_starts = 1;
    engine.tick(Shutdown::No).await;
    let runner = first_registered(&github);
    assert!(!cache_open(&cache, &runner), "its build cache is closed");
    assert_eq!(
        registrations(&github),
        Vec::new(),
        "its registration is deleted"
    );
    assert_eq!(engine.occupied().count(), 0, "its slot is free");
    assert!(!engine.any_starting(), "no start is left in flight");
    assert_eq!(held_for(&engine), Some(api::HoldCause::Start));
}

#[tokio::test]
async fn a_start_docker_fails_after_making_its_container_leaves_it_in_the_slot_uninspected() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle, cache) = job_queued_with_cache(&github, &docker, &dir).await;
    docker
        .0
        .lock()
        .unwrap()
        .late_failures
        .push_back(LateFailure {
            takes_job: false,
            cleanup_fails: true,
        });
    let traced = docker.0.lock().unwrap().trace.len();
    engine.tick(Shutdown::No).await;
    let runner = first_registered(&github);
    let held: Vec<String> = engine
        .occupied()
        .map(|(_, occupant)| occupant.container.clone())
        .collect();
    assert_eq!(held.len(), 1, "the container Docker kept holds the slot");
    assert!(
        held[0].starts_with("late-"),
        "the slot holds the container Docker kept: {held:?}"
    );
    assert!(cache_open(&cache, &runner), "its build cache stays with it");
    let inspected = docker.0.lock().unwrap().trace[traced..]
        .iter()
        .any(|call| *call == format!("inspect {}", held[0]));
    assert!(!inspected, "a failed start is not watched as a runner");
    assert_eq!(held_for(&engine), Some(api::HoldCause::Start));
}

#[tokio::test]
async fn a_runner_whose_first_inspect_fails_is_placed_unwatched_and_watched_on_the_next_tick() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle) = job_queued(&github, &docker, &dir).await;
    docker.0.lock().unwrap().fail_started_inspects = 1;
    engine.tick(Shutdown::No).await;
    let runner = first_registered(&github);
    let held: Vec<String> = engine
        .occupied()
        .map(|(_, occupant)| occupant.runner_name.clone())
        .collect();
    assert_eq!(
        held,
        std::slice::from_ref(&runner),
        "its runner holds the slot"
    );
    assert!(
        !engine.watches.is_watched(&runner),
        "Docker said nothing to watch it by"
    );
    engine.tick(Shutdown::No).await;
    assert!(
        engine.watches.is_watched(&runner),
        "the next tick watches it"
    );
}

#[tokio::test]
async fn a_start_in_flight_is_planned_as_a_busy_runner() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    let admission = api::Admission {
        policy: api::AdmissionPolicy::Caps,
        reserved: 4 * G,
        basis: api::Basis::Cap,
        free: None,
        slice: None,
        cpu: None,
    };
    let starting = Starting::new("o/r", "runnerd-ci-app-1-1", admission, 4 * G, 2000);
    engine.slot_mut(&slot_1()).launch(starting);
    engine.set_listed("o/r", vec![job(1)]);
    let (plan, _) = engine.plan();
    let started: Vec<usize> = plan
        .decisions
        .iter()
        .filter_map(|decision| match decision {
            Decision::Start { slot, .. } => Some(slot.index + 1),
            Decision::Stop { .. } => None,
        })
        .collect();
    assert_eq!(started, [2], "job 1 gets slot 2, as slot 1 is starting");
}

#[tokio::test]
async fn a_start_whose_task_panics_panics_its_tick() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle) = job_queued(&github, &docker, &dir).await;
    docker.0.lock().unwrap().panic_starts = 1;
    let ticked = tokio::time::timeout(
        Duration::from_secs(30),
        std::panic::AssertUnwindSafe(engine.tick(Shutdown::No)).catch_unwind(),
    )
    .await;
    assert!(
        matches!(ticked, Ok(Err(_))),
        "the tick panics as its start did, rather than leaving the slot starting"
    );
}
