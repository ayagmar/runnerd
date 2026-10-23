//! Measured admission on the fakes: more runners than the caps fit when the history predicts small peaks, nothing
//! while the slice is at the high-water mark, the caps without a recent slice sample or after a kill for memory,
//! and the decision in each runner's record.

use super::*;
use crate::api::{AdmissionPolicy, Basis};
use crate::store::{RunnerRecord, SampleRecord};
use crate::time;

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;

/// `app` 4 slots of 4G under measured admission: 16G of caps for a 10G slice.
fn measured() -> String {
    CONFIG.replace("slots = 2", "slots = 4").replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\nadmission = \"measured\"",
    )
}

/// Ten finished `build` jobs of o/r that peaked at `peak`, the last one with `oom_kills` kills for memory.
async fn history(store: &Store, peak: u64, oom_kills: u64) {
    history_of(store, "build", 1000, peak, oom_kills).await;
}

/// Ten finished `name` jobs of o/r from ID `first`, as `history`.
async fn history_of(store: &Store, name: &str, first: u64, peak: u64, oom_kills: u64) {
    let at = time::now_ms() - 3600 * 1000;
    for i in 0..10 {
        let id = first + i;
        let runner = format!("earlier-{id}");
        store.queued(
            "o/r",
            &QueuedJob {
                created_at: at,
                name: name.to_owned(),
                ..job(id)
            },
        );
        store.runner_started(RunnerRecord {
            name: runner.clone(),
            repository: "o/r".to_owned(),
            profile: "app".to_owned(),
            slot: 1,
            container: format!("c-{id}"),
            started_at: at,
            memory_cap: Some(4 * G),
            cpu_cap: Some(2000),
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
            oom_kills: if i == 9 { oom_kills } else { 0 },
        }]);
        store.lost(id, at + 60_000);
        store.runner_removed(&runner, at + 60_000);
    }
    store.flush().await;
}

/// The slice's memory, sampled now.
fn slice_at(store: &Store, memory: u64) {
    store.host_sample(api::HostSample {
        at: time::now_ms() / 1000,
        cpu: 0,
        memory,
    });
}

/// An engine under measured admission with a 10G slice and the history in place, four build jobs queued.
async fn measured_engine(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    peak: u64,
    oom_kills: u64,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let (mut engine, handle) = engine_with(github, docker, dir, &measured());
    engine.memory_max = Some(10 * G);
    history(&handle.store, peak, oom_kills).await;
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    (engine, handle)
}

#[tokio::test]
async fn small_predicted_peaks_start_more_runners_than_the_caps_fit() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    // At 4G caps the 10G slice holds 2; 1G peaks + 15% reserve 1.25G each, so all 4 start.
    assert_eq!(registered(&github.calls()).len(), 4);
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 4 * 1280 * M);
    assert_eq!(
        handle.status.borrow().budget.admission,
        AdmissionPolicy::Measured
    );
}

#[tokio::test]
async fn nothing_starts_while_the_slice_is_at_the_high_water_mark() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    // 9.2G of 10G: past 90%, whatever the predictions leave.
    slice_at(&handle.store, 92 * G / 10);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 0);
    engine.publish();
    assert!(
        handle.status.borrow().budget.held,
        "the budget says admission is held"
    );
    slice_at(&handle.store, 3 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4);
    engine.publish();
    assert!(!handle.status.borrow().budget.held, "admission resumed");
}

#[tokio::test]
async fn without_a_recent_sample_of_the_slice_runners_are_admitted_at_their_caps() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    handle.store.host_sample(api::HostSample {
        at: time::now_ms() / 1000 - 60,
        cpu: 0,
        memory: 2 * G,
    });
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2, "2 × 4G fit in 10G");
}

#[tokio::test]
async fn a_job_killed_for_memory_is_admitted_at_its_cap() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 1).await;
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "its peaks say nothing for a week"
    );
}

#[tokio::test]
async fn a_jobs_record_says_how_its_runner_was_admitted() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    github.0.lock().unwrap().queued = vec![job(1)];
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    let runner = registered(&github.calls())[0]
        .trim_start_matches("register ")
        .to_owned();
    handle.store.claimed(1, 1, &runner, time::now_ms());
    handle.store.flush().await;
    let admission = handle.store.job(1).await.unwrap().unwrap().admission;
    assert_eq!(
        admission,
        Some(api::Admission {
            policy: AdmissionPolicy::Measured,
            reserved: 1280 * M,
            basis: Basis::Job { runs: 10 },
            free: Some(10 * G),
            slice: Some(2 * G),
        })
    );
}

/// A sample of each runner the engine registered, using `memory`.
fn runners_use(github: &FakeGithub, store: &Store, memory: u64) {
    let samples = registered(&github.calls())
        .iter()
        .map(|call| SampleRecord {
            runner: call.trim_start_matches("register ").to_owned(),
            at: time::now_ms(),
            cpu: 0,
            cpu_usage: 0,
            memory,
            network_received: 0,
            network_sent: 0,
            oom_kills: 0,
        })
        .collect();
    store.samples(samples);
}

#[tokio::test]
async fn a_runner_using_more_than_it_reserved_counts_at_what_it_uses() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
    // Both reserved 1.25G and use 4G: the 10G slice has 2G left, under the 9G mark.
    runners_use(&github, &handle.store, 4 * G);
    slice_at(&handle.store, 8 * G);
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        3,
        "one more 1.25G runner fits in the 2G left, not two"
    );
    engine.publish();
    assert_eq!(
        handle.status.borrow().budget.memory_in_use,
        8 * G + 1280 * M
    );
}

#[tokio::test]
async fn back_under_caps_every_runner_counts_at_its_cap_whatever_it_reserved() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2, "2 × 1.25G reserved");
    // Back to caps with 3 × 3G, which fits the 10G slice; the two running containers keep their 4G caps.
    let caps = CONFIG
        .replace("slots = 2", "slots = 3")
        .replace("memory = \"4G\"", "memory = \"3G\"");
    engine.reload(Config::parse(&caps).unwrap(), Some(10 * G));
    github.0.lock().unwrap().queued = (1..=3).map(job).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "8G of caps run: a 3G runner does not fit"
    );
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 8 * G);
}

#[tokio::test]
async fn once_its_job_is_known_a_runner_reserves_that_jobs_prediction() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    // Started for a build, the runner gets job 9, an e2e another runner's build left behind.
    handle.store.queued(
        "o/r",
        &QueuedJob {
            name: "e2e".to_owned(),
            ..job(9)
        },
    );
    docker
        .0
        .lock()
        .unwrap()
        .captures
        .insert(("1".to_owned(), "0".to_owned()), "@@job 9 1\n".to_owned());
    github.0.lock().unwrap().queued = vec![job(1)];
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    until_state(&handle, 9, JobState::Running).await;
    github.0.lock().unwrap().queued = Vec::new();
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    engine.publish();
    // 3G + 15% is 3.45G, rounded up to 3.5G.
    assert_eq!(handle.status.borrow().budget.memory_in_use, 3584 * M);
    handle.store.flush().await;
    let admission = handle.store.job(9).await.unwrap().unwrap().admission;
    assert_eq!(
        admission.map(|admission| (admission.reserved, admission.basis)),
        Some((3584 * M, Basis::Job { runs: 10 }))
    );
}

#[tokio::test]
async fn an_idle_runner_reserves_the_largest_prediction_of_the_jobs_queued_since() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    github.0.lock().unwrap().queued = vec![job(1)];
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1, "1.25G for the build");
    // An e2e queues beside the build; GitHub may give either to the waiting runner.
    github.0.lock().unwrap().queued = vec![
        job(1),
        QueuedJob {
            name: "e2e".to_owned(),
            ..job(2)
        },
    ];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
    engine.publish();
    assert_eq!(
        handle.status.borrow().budget.memory_in_use,
        2 * 3584 * M,
        "both runners reserve the e2e's 3.5G"
    );
}
