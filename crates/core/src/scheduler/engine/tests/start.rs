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
async fn a_start_in_flight_counts_its_reservation_under_measured_admission_and_its_cap_under_caps()
{
    // Catches a start in flight read as a stale runner at its cap while its registration waits (ADR 0130), and one
    // that counts other than the runner it becomes while the slice's sample is recent.
    let measured = CONFIG.replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\nadmission = \"measured\"",
    );
    let caps = under_caps(CONFIG);
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
            api::AdmissionPolicy::Caps => &caps,
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

        // In flight, measured admission goes by its reservation, which has no container to sample; placed, by its
        // reservation only beside a recent sample of the slice, else by its cap.
        let (flying, landed) = match (policy, sampled) {
            (api::AdmissionPolicy::Caps, _) => (4 * G, 4 * G),
            (api::AdmissionPolicy::Measured, Sampled::Fresh) => (G, G),
            (api::AdmissionPolicy::Measured, Sampled::Stale) => (G, 4 * G),
        };
        assert_eq!(in_flight.count.plain, flying, "{case}");
        assert_eq!(placed.count.plain, landed, "{case}");
        if flying == landed {
            assert_eq!(in_flight, placed, "{case}: a start counts as its runner");
        }
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

/// An engine with room for two 4G `o/r` runners and a 512M `o/s` one, and jobs 1 and 2 of `o/r` and job 3 of `o/s`
/// queued, in that order. Docker fails the first runner's start after making its container, and once it is admitted,
/// before the second launches, other programs take the host's memory down to what the small runner fits in, not a 4G
/// one.
async fn three_jobs_whose_first_start_fails_as_the_host_fills(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let config = sized("0G", "4G", "512M").replacen("slots = 1", "slots = 2", 1);
    let (mut engine, handle) = engine_with(github, docker, dir, &config);
    engine.memory_max = Some(20 * G);
    engine.recover().await.unwrap();
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1), job(2), small_job(3)];
        state
            .queued_on
            .extend([(1, "o/r".to_owned()), (2, "o/r".to_owned())]);
        state.queued_on.insert(3, "o/s".to_owned());
    }
    (engine, handle)
}

fn fail_the_first_start_as_the_host_fills(docker: &FakeDocker, handle: &Handle) {
    let store = handle.store.clone();
    let mut state = docker.0.lock().unwrap();
    state.late_failures.push_back(LateFailure {
        takes_job: false,
        cleanup_fails: false,
    });
    state.on_prepare = Some(Box::new(move |_| {
        // Less job 1's 4G in flight and the host's margin, 1G is left: job 3's 512M fits, job 2's 4G does not.
        admission::host_at(&store, 0, 9 * G, 0);
    }));
}

fn small_registered(github: &FakeGithub) -> bool {
    registered(&github.calls())
        .iter()
        .any(|call| call.contains("-small-"))
}

/// The host fills as job 1's runner launches: job 2's no longer fits beside it, and job 3's counted on the room job
/// 2's took in the plan: it waits for the next plan, though it fits. The bug it catches: a deferred start's later ones launching anyway, so a
/// younger job takes the room an older one waits for. A tick plans again only after a plan that did something, and
/// a failed start does nothing.
#[tokio::test]
async fn a_tick_starts_none_of_its_plans_starts_after_one_that_no_longer_fits() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) =
        three_jobs_whose_first_start_fails_as_the_host_fills(&github, &docker, &dir).await;
    admission::host_at(&handle.store, 0, 20 * G, 0);
    fail_the_first_start_as_the_host_fills(&docker, &handle);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1, "{:?}", github.calls());
    assert!(
        !small_registered(&github),
        "job 3 waits behind job 2: {:?}",
        github.calls()
    );
}

/// As above, for a pass: the jobs whose starts it deferred go on waiting for the host's memory, as they did before
/// it. The bugs it catches: a pass launching a deferred start's later ones, and a pass ending the waits of jobs it
/// did not start.
#[tokio::test]
async fn a_pass_starts_none_of_its_plans_starts_after_one_that_no_longer_fits_and_its_jobs_go_on_waiting()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) =
        three_jobs_whose_first_start_fails_as_the_host_fills(&github, &docker, &dir).await;
    // Too little memory for any runner: all three wait for it.
    admission::host_at(&handle.store, 0, 2 * G, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 0, "nothing starts");
    assert!(
        (1..=3).all(|job| engine.waits_for_the_host(job)),
        "every job waits for the host's memory"
    );
    admission::host_at(&handle.store, 0, 20 * G, 0);
    fail_the_first_start_as_the_host_fills(&docker, &handle);
    assert_eq!(engine.refill().await, 0, "job 1's runner failed to start");
    assert_eq!(registered(&github.calls()).len(), 1, "{:?}", github.calls());
    assert!(
        !small_registered(&github),
        "job 3 waits behind job 2: {:?}",
        github.calls()
    );
    assert!(
        engine.waits_for_the_host(2) && engine.waits_for_the_host(3),
        "jobs 2 and 3 go on waiting for the host's memory"
    );
}

/// Jobs `ids` of `repository` queued, on a strict GitHub.
fn queue(github: &FakeGithub, repository: &str, ids: impl IntoIterator<Item = u64>) {
    let mut state = github.0.lock().unwrap();
    state.strict = true;
    for id in ids {
        state.queued.push(job(id));
        state.queued_on.insert(id, repository.to_owned());
    }
}

/// The runner containers Docker made for `repository`.
fn containers_of(docker: &FakeDocker, repository: &str) -> usize {
    docker
        .0
        .lock()
        .unwrap()
        .containers
        .iter()
        .filter(|container| {
            container
                .labels
                .get("runnerd.repository")
                .map(String::as_str)
                == Some(repository)
        })
        .count()
}

/// Five jobs of `o/r`. The first runner registers, and GitHub holds the next registration until the first runner's
/// container starts; then it fails it. Its failure comes back while the engine folds the first start, which reads
/// the history. The pool is held from then on: the fifth start, which launches only once a start is folded, does not
/// launch, while those in flight finish. The bug it catches: a launch that goes by the starts it folded, not by all
/// that came back, so one failure that came back while the engine was busy lets one more start into a held pool.
#[tokio::test]
async fn a_failure_that_comes_back_while_the_engine_folds_another_start_holds_back_the_next_launch()
{
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let config = CONFIG.replace("slots = 2", "slots = 5");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    queue(&github, "o/r", 1..=5);
    {
        let mut state = github.0.lock().unwrap();
        state.pass_before_fault = 1;
        state.fail_registrations = 1;
        state.on_register = Some(Box::new(|state| {
            state.held_registrations.send_replace(true);
        }));
    }
    let released = github.clone();
    docker.0.lock().unwrap().on_start = Some(Box::new(move |_| {
        released.release_registrations();
    }));
    engine.tick(Shutdown::No).await;
    // Three when the failure comes back before the fourth launch, as the history's write of a name may let it.
    let launched = engine.launches().count();
    assert!(
        launched < 5,
        "the fifth start does not launch: {:?}",
        github.calls()
    );
    assert_eq!(registered(&github.calls()).len(), launched);
    assert_eq!(held_for(&engine), Some(api::HoldCause::Register));
}

/// Three jobs of `o/r`. GitHub fails the first registration, which comes back while the second start's launch waits
/// for its caches and the history's write of its name: the pool is held by then, and neither that start nor the third
/// registers. The bug it catches: a launch that looks at what came back only before its own waits, so one more
/// registration goes into a pool a failure already holds.
#[tokio::test]
async fn a_failure_that_comes_back_while_a_launch_waits_holds_back_that_start() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let config = CONFIG.replace("slots = 2", "slots = 3");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    queue(&github, "o/r", 1..=3);
    github.0.lock().unwrap().fail_registrations = 1;
    // The first registration fails while the second start's caches are prepared, however loaded the machine.
    docker.0.lock().unwrap().latency.prepare = Duration::from_millis(20);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "only the failed registration: {:?}",
        github.calls()
    );
    assert_eq!(held_for(&engine), Some(api::HoldCause::Register));
    assert!(!engine.any_starting(), "no slot is left starting");
}

/// Three jobs of `o/r`; a detaching stop is asked for as the second start's caches are prepared. That start opens
/// nothing: no slot but the first start's is taken, and no build cache is open but its runner's; the third is not
/// even prepared. The bugs it catches: a launch going on past a stop that came during its own waits, so it opens a
/// build cache and starts a task after the stop; and a launch that looks for a stop only once it waited for Docker
/// and the history.
#[tokio::test]
async fn a_stop_asked_for_while_a_launch_waits_opens_nothing_of_that_start() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let config = CONFIG.replace("slots = 2", "slots = 3");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    queue(&github, "o/r", 1..=3);
    let control = handle.control.clone();
    {
        let mut state = docker.0.lock().unwrap();
        state.prepare_hook_after = 1;
        state.on_prepare = Some(Box::new(move |_| {
            control.shutdown(Shutdown::Detach);
        }));
    }
    engine.tick(Shutdown::No).await;
    let launched = engine.launches().after(0);
    assert_eq!(launched.len(), 1, "only the first start launches");
    assert_eq!(
        docker.0.lock().unwrap().prepares,
        2,
        "the third start is not prepared"
    );
    let taken = engine.slots["app"]
        .iter()
        .filter(|slot| !slot.is_free())
        .count();
    assert!(
        taken <= 1,
        "no slot but the first start's is taken: {taken}"
    );
    assert!(
        engine
            .build_cache_runners()
            .iter()
            .all(|runner| launched.contains(runner)),
        "no build cache open but the first runner's: {:?}",
        engine.build_cache_runners()
    );
}

/// Six jobs wait for the host's memory. It frees, and a pass launches them, four at a time; GitHub refuses the first
/// registration for its used-up rate limit, and the client then fails every request at once. The pass launches
/// nothing once the refusal is back, and every job goes on waiting as it did, the refused one too. The bugs it catches: a pass that checks
/// the limit only as it begins, preparing caches and naming runners for starts that cannot register; and a refused
/// registration counted as a start done with, which ends its job's wait.
#[tokio::test]
async fn a_pass_launches_nothing_once_github_refuses_a_registration_for_the_rate_limit() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let config = CONFIG.replace("slots = 2", "slots = 6");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.memory_max = Some(64 * G);
    engine.recover().await.unwrap();
    queue(&github, "o/r", 1..=6);
    admission::host_at(&handle.store, 0, 2 * G, 0);
    engine.tick(Shutdown::No).await;
    assert!(
        (1..=6).all(|job| engine.waits_for_the_host(job)),
        "every job waits for the host's memory"
    );
    admission::host_at(&handle.store, 0, 60 * G, 0);
    github.0.lock().unwrap().limit_registrations = 1;
    assert_eq!(engine.refill().await, 0, "nothing started");
    let launched = engine.launches().count();
    assert!(
        (1..=4).contains(&launched),
        "only starts launched before the refusal came back, at most four: {launched}"
    );
    assert_eq!(registered(&github.calls()).len(), 1, "{:?}", github.calls());
    assert!(
        engine.github_limit().is_some(),
        "no runner starts until it resets"
    );
    assert!(
        (1..=6).all(|job| engine.waits_for_the_host(job)),
        "every job goes on waiting"
    );
}

/// Five jobs of `o/r` and then one of `o/s`, in one profile, through a pass. GitHub takes 0.3 s a registration and
/// fails `o/r`'s second, while the first runner's container is still starting: `o/r`'s pool is held, the two other
/// `o/r` starts in flight finish, its fifth does not launch, and `o/s`'s does, in the same pass. The bugs it catches: a
/// start launching into a pool held since the plan, and that pool's hold keeping another pool's start waiting for the
/// next plan.
#[tokio::test(start_paused = true)]
async fn a_pool_held_mid_batch_launches_nothing_more_and_another_pools_start_goes_ahead() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let config = two_repositories().replace("slots = 2", "slots = 6");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().latency = Duration::from_millis(300);
    docker.0.lock().unwrap().latency.start = Duration::from_secs(1);
    engine.set_listed("o/r", (1..=5).map(job).collect());
    engine.set_listed("o/s", vec![job(6)]);
    {
        let mut state = github.0.lock().unwrap();
        state.strict = true;
        state.pass_before_fault = 1;
        state.fail_registrations = 1;
    }
    assert_eq!(engine.refill().await, 4);
    assert_eq!(registered(&github.calls()).len(), 5, "{:?}", github.calls());
    assert_eq!(containers_of(&docker, "o/r"), 3, "jobs 1, 3 and 4");
    assert_eq!(containers_of(&docker, "o/s"), 1, "o/s's pool goes on");
    assert_eq!(held_for(&engine), Some(api::HoldCause::Register));
}

/// A job of `o/r` and one of `o/s`, each its own pool. Docker fails the first runner's start without making its
/// container, and GitHub does not answer deletes. The other pool's runner starts all the same and the tick ends; the
/// failed start's registration goes once GitHub answers, and a stop waits for that. The bug it catches: the fold
/// awaiting the delete, which holds every other start of its batch, and the tick, behind GitHub.
#[tokio::test]
async fn a_failed_starts_registration_is_deleted_without_holding_back_its_batch() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("start"),
    );
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &two_repositories());
    engine.recover().await.unwrap();
    queue(&github, "o/r", [1]);
    queue(&github, "o/s", [2]);
    docker.0.lock().unwrap().fail_starts = 1;
    github.hold_deletes();
    let ticked = tokio::time::timeout(Duration::from_secs(30), engine.tick(Shutdown::No)).await;
    assert!(
        ticked.is_ok(),
        "the tick ends while GitHub holds the delete"
    );
    assert_eq!(
        runner_containers(&docker).len(),
        1,
        "the other pool's runner runs"
    );
    assert_eq!(
        registrations(&github).len(),
        2,
        "the delete waits for GitHub"
    );
    github.release_deletes();
    engine.finish_deletes().await;
    assert_eq!(
        registrations(&github)
            .iter()
            .map(|runner| runner.name.clone())
            .collect::<Vec<_>>(),
        runner_containers(&docker),
        "only the running runner stays registered"
    );
}
