//! The slots' Docker stores sized per profile (ADR 0156): what a runner's log reports of its slot's store is kept
//! with its runner, and the next runners' shares follow it, after a restart too.

use super::*;
use crate::cache::shares::{StoreReport, StoreUse};
use crate::store::RunnerRecord;

const G: u64 = 1 << 30;

/// `app` and `small`, two slots each: with 48 GiB free, 3 GiB a slot split equally.
fn two_profiles() -> String {
    sized("512M", "4G", "1G").replace("slots = 1", "slots = 2")
}

/// The `RUNNERD_DOCKER_BUDGET_GB` of every runner started, in order.
fn budgets(docker: &FakeDocker) -> Vec<String> {
    let state = docker.0.lock().unwrap();
    state
        .envs
        .values()
        .flatten()
        .filter(|var| var.starts_with("RUNNERD_DOCKER_BUDGET_GB="))
        .cloned()
        .collect()
}

/// Ticks until the measure of Docker's disk the recovery started is in.
async fn until_measured(engine: &mut Engine<FakeGithub, FakeDocker>) {
    for _ in 0..50 {
        if engine.docker_disk.borrow().is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    panic!("Docker's disk was measured");
}

// Catches: the report read from a line a job printed, or from an image older than the report, as if it measured.
#[tokio::test]
async fn a_runners_last_store_line_is_kept_with_it_and_an_older_image_reports_nothing() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_job(&github, &docker, "o/r", 1, 1);
    running_job(&github, &docker, "o/r", 2, 2);
    {
        let mut state = docker.0.lock().unwrap();
        state.container_logs.insert(
            "container-1".to_owned(),
            "Running job: build\nrunnerd: docker store held=1 share=20\n\
             Job build completed with result: Succeeded\nrunnerd: docker store held=5368709120 share=3\n"
                .to_owned(),
        );
        state.container_logs.insert(
            "container-2".to_owned(),
            "Running job: build\nJob build completed with result: Succeeded\n".to_owned(),
        );
    }
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    until_state(&handle, 2, JobState::Running).await;
    for container in &mut docker.0.lock().unwrap().containers {
        container.running = false;
    }
    engine.tick(Shutdown::No).await;
    handle.store.flush().await;
    assert_eq!(
        handle.store.docker_stores(0).await.unwrap(),
        vec![StoreUse {
            profile: "app".to_owned(),
            held: 5 * G
        }]
    );
}

// Catches: shares kept only in memory, so a restart splits equally until jobs report again; and a share that follows
// the profile's own needs without giving the others' unused room to the one that overflows.
#[tokio::test]
async fn after_a_restart_the_profile_whose_stores_held_more_gets_the_room_the_other_leaves() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().free = Some(48 * G);
    let dir = TestDir::new("shares");
    let config = two_profiles();
    {
        // An earlier daemon's runners: app's stores held 6 GiB after each job, small's a fifth of one.
        let (_engine, handle) = engine_with(&github, &docker, &dir, &config);
        let now = crate::time::now_ms();
        for (index, (profile, held)) in [("app", 6 * G), ("small", G / 5)]
            .into_iter()
            .flat_map(|use_| std::iter::repeat_n(use_, 5))
            .enumerate()
        {
            let name = format!("earlier-{index}");
            handle.store.runner_started(RunnerRecord {
                name: name.clone(),
                repository: "o/r".to_owned(),
                profile: profile.to_owned(),
                slot: 1,
                container: format!("gone-{index}"),
                started_at: now - 3_600_000 + index as u64,
                memory_cap: None,
                cpu_cap: None,
                admission: None,
                purpose: None,
            });
            handle
                .store
                .runner_docker_store(&name, StoreReport { held, share_gib: 3 });
        }
        handle.store.flush().await;
    }
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    until_measured(&mut engine).await;
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1), small_job(2)];
        state.jobs.insert(1, details(1, "in_progress", None));
        state.jobs.insert(2, details(2, "in_progress", None));
    }
    engine.tick(Shutdown::No).await;
    let mut found = budgets(&docker);
    found.sort();
    // small keeps the floor, 1 GiB a slot; app's two slots take the other 10.
    assert_eq!(
        found,
        ["RUNNERD_DOCKER_BUDGET_GB=1", "RUNNERD_DOCKER_BUDGET_GB=5"],
        "small at its floor, app above the equal 3"
    );
}

// Catches: a history that cannot be read leaving runners without a share, or with a share of nothing.
#[tokio::test]
async fn without_reports_every_slot_keeps_the_equal_share() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().free = Some(48 * G);
    let dir = TestDir::new("shares-equal");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &two_profiles());
    engine.recover().await.unwrap();
    until_measured(&mut engine).await;
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1), small_job(2)];
        state.jobs.insert(1, details(1, "in_progress", None));
        state.jobs.insert(2, details(2, "in_progress", None));
    }
    engine.tick(Shutdown::No).await;
    assert_eq!(
        budgets(&docker),
        ["RUNNERD_DOCKER_BUDGET_GB=3", "RUNNERD_DOCKER_BUDGET_GB=3"]
    );
}
