//! A repository served by two pools (ADR 0058): a light job starts in the light pool while a heavy one waits, the
//! light pool reserves only what its own jobs predict, and a job no pool's labels match says so.

use super::*;
use crate::store::{RunnerRecord, SampleRecord};
use crate::time;

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;

/// o/r in `app` (4G) for `[self-hosted, app]` jobs, and in `light` (1G) for `r-light` jobs.
fn pools() -> String {
    format!(
        "{CONFIG}
[profiles.light]
image_stage = \"app\"
cpus = 1
memory = \"1G\"
slots = 2

[[repositories]]
name = \"o/r\"
profile = \"light\"
labels = [\"r-light\"]
warm = 0
"
    )
}

/// A job of o/r asking only for `r-light`.
fn light(id: u64) -> QueuedJob {
    QueuedJob {
        name: "lint".to_owned(),
        labels: vec!["r-light".to_owned()],
        ..job(id)
    }
}

/// The runner of each slot that has one, as `(profile, labels it registered with, what it reserves)`.
fn runners(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    handle: &Handle,
    docker: &FakeDocker,
) -> Vec<Started> {
    engine.publish();
    let containers = docker.0.lock().unwrap().containers.clone();
    handle
        .status
        .borrow()
        .profiles
        .iter()
        .flat_map(|profile| {
            profile.slots.iter().filter_map(|slot| {
                let runner = slot.runner.as_ref()?;
                let labels = containers
                    .iter()
                    .find(|container| container.name == runner.name)
                    .and_then(|container| container.labels.get("runnerd.labels").cloned())
                    .unwrap_or_default();
                Some(Started {
                    profile: profile.name.clone(),
                    labels,
                    reserved: runner.reserved,
                })
            })
        })
        .collect()
}

/// A started runner as `runners` sees it.
#[derive(Debug, PartialEq)]
struct Started {
    profile: String,
    labels: String,
    reserved: u64,
}

#[tokio::test]
async fn a_light_job_starts_in_the_light_pool_while_a_heavy_one_waits_for_memory() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &pools());
    // Room for a light runner, not for a heavy one.
    engine.memory_max = Some(3 * G);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![job(1), light(2)];
    engine.tick(Shutdown::No).await;
    assert_eq!(
        runners(&mut engine, &handle, &docker),
        [Started {
            profile: "light".to_owned(),
            labels: "r-light".to_owned(),
            reserved: G,
        }]
    );
    let status = handle.status.borrow().clone();
    let heavy = &status.queued[0];
    assert_eq!(heavy.profile.as_deref(), Some("app"));
    assert!(
        matches!(heavy.waiting, Some(api::Waiting::Memory { needs, .. }) if needs == 4 * G),
        "the heavy job waits for 4G: {:?}",
        heavy.waiting
    );
    let pools: Vec<(&str, usize)> = status.repositories[0]
        .pools
        .iter()
        .map(|pool| (pool.profile.as_str(), pool.queued_jobs))
        .collect();
    // The light job stays listed until its runner takes it.
    assert_eq!(pools, [("app", 1), ("light", 1)]);
}

/// Ten finished `name` jobs of o/r in `profile`, asking for `labels`, from ID `first`, that peaked at `peak`.
async fn ran_in(store: &Store, profile: &str, labels: &[&str], name: &str, first: u64, peak: u64) {
    let at = time::now_ms() - 3600 * 1000;
    for id in first..first + 10 {
        let runner = format!("earlier-{id}");
        store.queued(
            "o/r",
            &QueuedJob {
                created_at: at,
                name: name.to_owned(),
                labels: labels.iter().map(|label| (*label).to_owned()).collect(),
                ..job(id)
            },
        );
        store.runner_started(RunnerRecord {
            name: runner.clone(),
            repository: "o/r".to_owned(),
            profile: profile.to_owned(),
            slot: 1,
            container: format!("c-{id}"),
            started_at: at,
            memory_cap: None,
            cpu_cap: None,
            admission: None,
        });
        store.claimed(id, 1, &runner, at + 1000);
        store.samples(vec![SampleRecord {
            runner: runner.clone(),
            at: at + 2000,
            cpu: 0,
            cpu_usage: 0,
            memory: peak,
            network_received: 0,
            network_sent: 0,
            events: crate::observe::metrics::Events::default(),
        }]);
        store.lost(id, at + 60_000);
        store.runner_removed(&runner, at + 60_000);
    }
    store.flush().await;
}

#[tokio::test]
async fn the_light_pool_reserves_only_what_its_own_queued_jobs_predict() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let config = pools().replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\nadmission = \"measured\"",
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.memory_max = Some(10 * G);
    ran_in(
        &handle.store,
        "app",
        &["self-hosted", "app"],
        "build",
        1000,
        3 * G,
    )
    .await;
    // The same job name in both pools: each pool's runs predict only its own.
    ran_in(&handle.store, "light", &["r-light"], "build", 2000, 200 * M).await;
    engine.recover().await.unwrap();
    handle.store.host_sample(api::HostSample {
        at: time::now_ms() / 1000,
        cpu: 0,
        memory: 2 * G,
    });
    let light_build = QueuedJob {
        name: "build".to_owned(),
        ..light(2)
    };
    github.0.lock().unwrap().queued = vec![job(1), light_build];
    engine.tick(Shutdown::No).await;
    let started = runners(&mut engine, &handle, &docker);
    let reserved = |profile: &str| {
        started
            .iter()
            .find(|runner| runner.profile == profile)
            .map(|runner| runner.reserved)
    };
    // Each its own job's peak and a margin: the light runner does not reserve for the build its pool cannot take.
    let (app, light) = (reserved("app").unwrap(), reserved("light").unwrap());
    assert!(
        app > 3 * G,
        "the build's runner reserves past its 3G peak: {app}"
    );
    assert!(
        light < 512 * M,
        "the light build's runner reserves for 200M, not the heavy build's 3G: {light}"
    );
}

#[tokio::test]
async fn a_job_whose_labels_no_pool_has_says_so_and_starts_nothing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &pools());
    engine.recover().await.unwrap();
    let mislabelled = QueuedJob {
        labels: vec!["r-lite".to_owned()],
        ..job(1)
    };
    // GitHub runs this one on its own runners: no pool is meant for it.
    let hosted = QueuedJob {
        labels: vec!["ubuntu-latest".to_owned()],
        ..job(2)
    };
    github.0.lock().unwrap().queued = vec![mislabelled, hosted];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
    engine.publish();
    let status = handle.status.borrow().clone();
    let waiting = status.queued[0].waiting.clone().unwrap();
    assert_eq!(
        waiting,
        api::Waiting::NoPool {
            labels: vec!["r-lite".to_owned()]
        }
    );
    assert_eq!(waiting.kind(), api::WaitKind::Act);
    assert_eq!(
        waiting.reason(&status.queued[0], &status, 0),
        "No profile serves the label r-lite"
    );
    assert_eq!(
        status.queued.len(),
        1,
        "the GitHub-hosted job is not listed"
    );
    assert_eq!(status.repositories[0].queued_jobs, 1);
}

#[tokio::test]
async fn pausing_a_repository_pauses_every_pool_of_it() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &pools());
    engine.recover().await.unwrap();
    engine.pause(&one("o/r"), RunningJobs::Keep).await.unwrap();
    github.0.lock().unwrap().queued = vec![job(1), light(2)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
    engine.publish();
    let status = handle.status.borrow().clone();
    assert_eq!(status.repositories.len(), 1, "one row for the repository");
    assert!(
        status
            .queued
            .iter()
            .all(|job| job.waiting == Some(api::Waiting::Paused)),
        "both jobs wait for the resume: {:?}",
        status.queued
    );
}

#[tokio::test]
async fn a_pool_whose_runners_fail_holds_back_only_itself() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    // The light pool's image is missing, so its runners cannot start.
    let config = pools().replacen(
        "image_stage = \"app\"\ncpus = 1",
        "image_stage = \"lite\"\ncpus = 1",
        1,
    );
    docker.0.lock().unwrap().missing_images = BTreeSet::from(["runnerd-runner:lite".to_owned()]);
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![light(1)];
    engine.tick(Shutdown::No).await;
    // A heavy job queued after the light pool's failure still gets its runner.
    github.0.lock().unwrap().queued = vec![light(1), job(2)];
    engine.tick(Shutdown::No).await;
    let started = runners(&mut engine, &handle, &docker);
    assert_eq!(
        started
            .iter()
            .map(|runner| runner.profile.as_str())
            .collect::<Vec<_>>(),
        ["app"]
    );
    let status = handle.status.borrow().clone();
    assert!(
        matches!(status.queued[0].waiting, Some(api::Waiting::Backoff { .. })),
        "the light job waits for its pool's backoff: {:?}",
        status.queued[0].waiting
    );
}

#[tokio::test]
async fn clients_and_daemons_before_pools_still_read_the_profile_of_a_repository() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &pools());
    engine.recover().await.unwrap();
    engine.publish();
    // An older client reads `profile` and `labels`: its first pool's.
    let json = serde_json::to_value(&handle.status.borrow().repositories[0]).unwrap();
    assert_eq!(json["profile"], "app");
    assert_eq!(json["labels"], serde_json::json!(["self-hosted", "app"]));
    // An older daemon sends no `pools`: its one profile is the pool.
    let older: api::RepositoryStatus = serde_json::from_value(serde_json::json!({
        "name": "o/r",
        "profile": "app",
        "labels": ["self-hosted"],
        "paused": false,
        "queued_jobs": 2,
    }))
    .unwrap();
    assert_eq!(older.profiles(), "app");
    // A reason added after this client still lets it read the job, said plainly.
    let later: api::QueuedJob = serde_json::from_value(serde_json::json!({
        "id": 1,
        "repository": "o/r",
        "run_id": 1,
        "run_attempt": 1,
        "name": "build",
        "queued_at": 0,
        "waiting": { "kind": "some-later-reason", "detail": 3 },
    }))
    .unwrap();
    let reason = later.waiting.clone().unwrap();
    assert_eq!(reason, api::Waiting::Unknown);
    assert_eq!(reason.kind(), api::WaitKind::Other);
    assert_eq!(
        reason.reason(&later, &handle.status.borrow(), 0),
        "Waits for a reason this client does not know yet"
    );
    assert_eq!(
        older.pool_for(&["self-hosted".to_owned()]).as_deref(),
        Some("app")
    );
}
