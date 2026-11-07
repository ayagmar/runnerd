//! Measured admission on the fakes: more runners than the caps fit when the history predicts small peaks, nothing
//! while the slice is at the high-water mark, the caps without a recent slice sample or after a kill for memory,
//! and the decision in each runner's record.

use super::*;
use crate::admission::pooled::POOL_AFTER;
use crate::api::{AdmissionPolicy, Basis, JobPeak};
use crate::observe::metrics::Events;
use crate::scheduler::PRIORITY_AFTER;
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
    runs_of(store, name, first, 10, peak, oom_kills, Ended::Lost).await;
}

/// How a run in `runs_of` ended.
#[derive(Clone, Copy, PartialEq)]
enum Ended {
    /// Its container was gone before GitHub reported it.
    Lost,
    /// GitHub reported it completed with success.
    Succeeded,
}

/// `count` finished `name` jobs of o/r from ID `first`, as `history`.
async fn runs_of(
    store: &Store,
    name: &str,
    first: u64,
    count: u64,
    peak: u64,
    oom_kills: u64,
    ended: Ended,
) {
    let at = time::now_ms() - 3600 * 1000;
    for i in 0..count {
        let id = first + i;
        let runner = format!("earlier-{id}");
        store.queued(
            "o/r",
            &QueuedJob {
                created_at: at,
                name: name.to_owned(),
                ..job(id)
            },
            time::now_ms(),
        );
        store.runner_started(RunnerRecord {
            purpose: None,
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
            cpu_held: crate::observe::metrics::CpuHeld::default(),
            events: Events {
                oom: Some(0),
                oom_kill: Some(if i == count - 1 { oom_kills } else { 0 }),
                sock_throttled: None,
                local_oom: None,
            },
        }]);
        match ended {
            Ended::Lost => store.lost(id, at + 60_000),
            // No times: the job keeps the ones it was queued and claimed at.
            Ended::Succeeded => store.details(JobDetails {
                id,
                run_id: 1,
                run_attempt: 1,
                name: name.to_owned(),
                workflow_name: None,
                head_branch: None,
                html_url: None,
                labels: Vec::new(),
                created_at: None,
                status: "completed".to_owned(),
                conclusion: Some("success".to_owned()),
                started_at: None,
                completed_at: None,
                runner_name: Some(runner.clone()),
                steps: Vec::new(),
            }),
        }
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
async fn a_reseed_waits_while_the_slice_is_at_the_high_water_mark() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(10 * G);
    engine.recover().await.unwrap();
    engine.request_reseed("app").unwrap();
    // 9.2G of 10G, though nothing runnerd counts holds any of it: the 4G seed container would push it to the cap.
    slice_at(&handle.store, 92 * G / 10);
    engine.tick(Shutdown::No).await;
    assert!(engine.reseeds.is_empty(), "the seed waits");
    slice_at(&handle.store, 3 * G);
    engine.tick(Shutdown::No).await;
    assert!(engine.reseeds.contains_key("app"), "the seed starts");
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
async fn peaks_sampled_with_page_cache_do_not_predict_what_a_job_reserves() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(10 * G);
    history(&handle.store, G, 0).await;
    // As a history from before version 8 holds them: those runners were sampled counting active page cache.
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .execute("UPDATE runners SET memory_measure = NULL", [])
        .unwrap();
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "no run measured the new way: 2 × the 4G cap"
    );
}

#[tokio::test]
async fn a_kill_for_memory_from_before_the_upgrade_keeps_its_job_on_its_cap() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(10 * G);
    // An hour ago, before the upgrade, the last build was killed for memory.
    history(&handle.store, G, 1).await;
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .execute("UPDATE runners SET memory_measure = NULL", [])
        .unwrap();
    // Since then the profile's light lint jobs ran, sampled the new way: enough to predict a new job from.
    history_of(&handle.store, "lint", 2000, G, 0).await;
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "builds reserve their 4G cap, not the lints' 1.25G"
    );
    // The start tasks write the runners' rows after the tick returns.
    let mut reserved: Vec<String> = Vec::new();
    for _ in 0..200 {
        handle.store.flush().await;
        reserved = rusqlite::Connection::open(dir.0.join("history.db"))
            .unwrap()
            .prepare("SELECT admission FROM runners WHERE name NOT LIKE 'earlier-%'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        if reserved.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Until a week after the kill, as without the upgrade; the history's times are a few ms older than this.
    let expires =
        (time::now_ms() - 3600 * 1000 + 60_000 + crate::admission::UNPREDICTABLE_FOR) / 1000;
    assert_eq!(reserved.len(), 2);
    for admission in reserved {
        let admission: api::Admission = serde_json::from_str(&admission).unwrap();
        assert!(
            matches!(admission.basis, Basis::Unpredictable { until } if until.abs_diff(expires) <= 1),
            "{:?}",
            admission.basis
        );
    }
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
            basis: Basis::Job {
                runs: 10,
                peak: JobPeak::P95
            },
            free: Some(10 * G),
            slice: Some(2 * G),
            cpu: None,
        })
    );
}

/// A sample of each runner the engine registered, using `memory`.
fn runners_use(github: &FakeGithub, store: &Store, memory: u64) {
    runners_used(github, store, memory, time::now_ms());
}

/// A sample of each runner the engine registered, using `memory` at `at` (ms).
fn runners_used(github: &FakeGithub, store: &Store, memory: u64, at: u64) {
    let samples = registered(&github.calls())
        .iter()
        .map(|call| SampleRecord {
            runner: call.trim_start_matches("register ").to_owned(),
            at,
            cpu: 0,
            cpu_usage: 0,
            memory,
            network_received: 0,
            network_sent: 0,
            cpu_held: crate::observe::metrics::CpuHeld::default(),
            events: Events::default(),
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
    for runner in &mut github.0.lock().unwrap().runners {
        runner.busy = true;
    }
    let caps = CONFIG
        .replace("slots = 2", "slots = 3")
        .replace("memory = \"4G\"", "memory = \"3G\"");
    engine.reload(Config::parse(&caps).unwrap(), Some(10 * G), None);
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
        time::now_ms(),
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
        Some((
            3584 * M,
            Basis::Job {
                runs: 10,
                peak: JobPeak::P95
            }
        ))
    );
}

#[tokio::test]
async fn a_runner_that_takes_a_job_no_listing_saw_reserves_that_jobs_prediction_once_github_names_it()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, 3 * G, 0).await;
    history_of(&handle.store, "lint", 2000, G, 0).await;
    // Started for a build, the runner gets job 9, a lint no listing of queued jobs showed.
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
    // GitHub does not answer for the job at first.
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    let mut lint = details(9, "in_progress", None);
    lint.name = "lint".to_owned();
    lint.workflow_name = None;
    github.0.lock().unwrap().jobs.insert(9, lint);
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    engine.publish();
    // 1G + 15%, rounded up, not the 3.5G of a build.
    assert_eq!(handle.status.borrow().budget.memory_in_use, 1280 * M);
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

#[tokio::test]
async fn waiting_runners_raised_past_the_budget_for_a_heavier_job_are_stopped_until_the_rest_fits()
{
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        4,
        "4 × 1.25G for the builds"
    );
    // An e2e queues: any of the four may get it, so each reserves its 3.5G, 14G of the 10G slice.
    github.0.lock().unwrap().queued.push(QueuedJob {
        name: "e2e".to_owned(),
        ..job(5)
    });
    engine.tick(Shutdown::No).await;
    let deleted: BTreeSet<String> = github
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("delete "))
        .collect();
    assert_eq!(deleted.len(), 2, "two stop, so the other two's 7G fits");
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 2 * 3584 * M);
    let starting = handle
        .status
        .borrow()
        .queued
        .iter()
        .filter(|job| job.waiting == Some(api::Waiting::RunnerStarting))
        .count();
    assert_eq!(
        starting, 2,
        "only the two runners left say they start a job"
    );
}

#[tokio::test]
async fn running_runners_count_at_their_caps_once_the_slice_has_no_recent_sample() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    runners_use(&github, &handle.store, G);
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 2 * 1280 * M);
    // The sampler stalled a minute ago: what the two use now is not known.
    handle.store.host_sample(api::HostSample {
        at: time::now_ms() / 1000 - 60,
        cpu: 0,
        memory: 2 * G,
    });
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 2 * 4 * G);
}

#[tokio::test]
async fn a_runner_whose_own_sample_is_old_counts_at_its_cap() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    github.0.lock().unwrap().queued = vec![job(1)];
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    // Its container's files could not be read for a minute; the slice is sampled now.
    runners_used(&github, &handle.store, G, time::now_ms() - 60_000);
    slice_at(&handle.store, 2 * G);
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 4 * G);
}

/// A burst of jobs ending left 20 runners counted at 88.8G with 7G in use on 2026-11-05: each stayed in its slot for
/// its log and its removal, past the sample that would have said it holds nothing, and counted at its cap.
#[tokio::test]
async fn a_runner_whose_container_is_gone_counts_nothing_while_it_keeps_its_slot() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    github.0.lock().unwrap().queued = vec![job(1)];
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    runners_used(&github, &handle.store, G, time::now_ms() - 60_000);
    slice_at(&handle.store, 2 * G);
    let runner = engine
        .occupied()
        .map(|(_, occupant)| occupant.runner_name.clone())
        .next()
        .unwrap();
    engine.publish();
    let counted = handle.status.borrow().budget.memory_in_use;
    handle.store.containers_gone(vec![runner.clone()]);
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 0);
    // A `docker restart` brings the same cgroup back: a live container's memory must not be spent twice.
    handle.store.containers_read(&[runner]);
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, counted);
}

#[tokio::test]
async fn two_light_runs_then_a_burst_of_heavy_copies_with_warm_up_never_overlap_past_memory_max() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(10 * G);
    // The profile's p95 is 3G; the build's two runs hit a warm cache and peaked at 200M.
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    runs_of(
        &handle.store,
        "build",
        1000,
        2,
        200 * M,
        0,
        Ended::Succeeded,
    )
    .await;
    engine.recover().await.unwrap();
    // The runners of slots 1 and 2 take builds 1 and 2.
    docker.0.lock().unwrap().slot_captures =
        HashMap::from([(1, "@@job 1 1\n".to_owned()), (2, "@@job 2 1\n".to_owned())]);
    // The cache is gone: four builds queue at once, each needing 3G once warmed up.
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    slice_at(&handle.store, G);
    engine.tick(Shutdown::No).await;
    // Copies of a job queued together reserve 3G + 15%, rounded up to 3.5G: two fit the 10G slice, not four at
    // 200M + 15%, rounded up to 256M.
    assert_eq!(registered(&github.calls()).len(), 2);
    // Their captures report their jobs.
    until_state(&handle, 1, JobState::Running).await;
    until_state(&handle, 2, JobState::Running).await;
    github.0.lock().unwrap().queued = Vec::new();
    slice_at(&handle.store, G);
    engine.tick(Shutdown::No).await;
    // Still warming up at 300M, both busy with known jobs. The first copy reserves its own 256M, the second 3.5G,
    // and the copies still queued 3.5G: one more fits, not two.
    {
        let mut state = github.0.lock().unwrap();
        for runner in &mut state.runners {
            (runner.status, runner.busy) = ("online".to_owned(), true);
        }
        state.queued = (3..=4).map(job).collect();
    }
    runners_use(&github, &handle.store, 300 * M);
    slice_at(&handle.store, G);
    engine.tick(Shutdown::No).await;
    let started = registered(&github.calls()).len() as u64;
    assert_eq!(started, 3);
    engine.publish();
    assert_eq!(
        handle.status.borrow().budget.memory_in_use,
        300 * M + 2 * 3584 * M
    );
    // Warmed up, the three copies' 3G peaks fit the 10G slice together; four would not.
    assert!(
        started * 3 * G <= 10 * G,
        "{started} copies at 3G fit the 10G slice"
    );
}

#[tokio::test]
async fn copies_arriving_one_by_one_beside_busy_runners_whose_job_is_unknown_take_their_profiles_p95()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(10 * G);
    // The profile's p95 is 3G; the build's two runs hit a warm cache and peaked at 200M.
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    runs_of(
        &handle.store,
        "build",
        1000,
        2,
        200 * M,
        0,
        Ended::Succeeded,
    )
    .await;
    engine.recover().await.unwrap();
    // No log capture tells which job a runner took.
    docker.0.lock().unwrap().fail_execs = u32::MAX;
    let mut started = Vec::new();
    for copy in 1..=4 {
        // The runners before took theirs, which a poll saw; they warm up at 300M, sampled just now. A runner first
        // listed busy may have taken a job the same poll still lists queued, so the copy queues after.
        {
            let mut state = github.0.lock().unwrap();
            for runner in &mut state.runners {
                (runner.status, runner.busy) = ("online".to_owned(), true);
            }
            state.queued.clear();
        }
        engine.tick(Shutdown::No).await;
        github.0.lock().unwrap().queued = vec![job(copy)];
        runners_use(&github, &handle.store, 300 * M);
        slice_at(&handle.store, G);
        engine.tick(Shutdown::No).await;
        started.push(registered(&github.calls()).len());
    }
    // The first reserves 256M, alone. The second may be a copy of the busy runner's build, so it reserves
    // 3G + 15%, rounded up to 3.5G, and so does the first once a build queues beside it: the 10G slice holds no
    // third, where 256M each would let all four start.
    assert_eq!(started, [1, 2, 2, 2]);
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 2 * 3584 * M);
}

#[tokio::test]
async fn jobs_wait_for_cpu_the_budget_lacks_and_their_records_say_what_they_reserved_and_waited() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let config = measured().replace(
        "admission = \"measured\"",
        "admission = \"measured\"\ncpu_overcommit = 1.0",
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.memory_max = Some(10 * G);
    engine.cpus = Some(4000);
    history(&handle.store, G, 0).await;
    // Each build used 1.5 CPUs over its run.
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .execute("UPDATE jobs SET cpu_average = 1500", [])
        .unwrap();
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    slice_at(&handle.store, 2 * G);

    engine.tick(Shutdown::No).await;
    // Memory fits all 4 at 1.25G each; 4 CPUs fit 2 builds at 1.5.
    let first = registered(&github.calls())
        .iter()
        .map(|call| call.trim_start_matches("register ").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(first.len(), 2);
    engine.publish();
    let cpu = handle.status.borrow().budget.cpu;
    assert_eq!(
        cpu,
        Some(api::CpuBudget {
            cpus: 4000,
            budget: 4000,
            in_use: 3000,
            held: true,
        })
    );

    handle.store.claimed(1, 1, &first[0], time::now_ms());
    handle.store.flush().await;
    let admission = handle.store.job(1).await.unwrap().unwrap().admission;
    assert_eq!(
        admission.and_then(|admission| admission.cpu),
        Some(api::CpuAdmission {
            reserved: 1500,
            basis: Basis::Job {
                runs: 10,
                peak: JobPeak::P95,
            },
            free: 4000,
            waited: None,
        })
    );

    // The slice gets more CPUs: the other two start, and say they waited for CPU.
    engine.cpus = Some(8000);
    engine.tick(Shutdown::No).await;
    let all = registered(&github.calls())
        .iter()
        .map(|call| call.trim_start_matches("register ").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(all.len(), 4);
    handle.store.claimed(3, 1, &all[2], time::now_ms());
    handle.store.flush().await;
    let admission = handle.store.job(3).await.unwrap().unwrap().admission;
    assert!(
        admission
            .and_then(|admission| admission.cpu)
            .is_some_and(|cpu| cpu.waited.is_some()),
        "the third build's record says it waited for CPU: {admission:?}"
    );
    engine.publish();
    assert!(
        handle
            .status
            .borrow()
            .budget
            .cpu
            .is_some_and(|cpu| !cpu.held),
        "nothing waits for CPU any more"
    );
}

#[tokio::test]
async fn a_job_with_only_runs_from_before_the_upgrade_reserves_its_profiles_largest_peak_with_its_cpu_recorded()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    // No cpu_overcommit: admission is by memory alone.
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(10 * G);
    history(&handle.store, G, 0).await;
    // `legacy` ran only before the upgrade: its peaks counted page cache, its CPU was recorded as ever.
    history_of(&handle.store, "legacy", 2000, 3 * G, 0).await;
    let db = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    db.execute(
        "UPDATE runners SET memory_measure = NULL WHERE name LIKE 'earlier-2%'",
        [],
    )
    .unwrap();
    db.execute("UPDATE jobs SET cpu_average = 1500", [])
        .unwrap();
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![QueuedJob {
        name: "legacy".to_owned(),
        ..job(1)
    }];
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    let runner = registered(&github.calls())[0]
        .trim_start_matches("register ")
        .to_owned();
    handle.store.claimed(1, 1, &runner, time::now_ms());
    handle.store.flush().await;
    let admission = handle
        .store
        .job(1)
        .await
        .unwrap()
        .unwrap()
        .admission
        .unwrap();
    // The builds' 1G largest peak + 15%, as a job never seen: not its 4G cap.
    assert_eq!(
        (admission.reserved, admission.basis, admission.cpu),
        (1280 * M, Basis::ProfileMax { runs: 10 }, None)
    );
}

/// `measured()` with 6 slots and a CPU budget of the slice's CPUs.
fn cpu_measured() -> String {
    measured().replace("slots = 4", "slots = 6").replace(
        "admission = \"measured\"",
        "admission = \"measured\"\ncpu_overcommit = 1.0",
    )
}

#[tokio::test]
async fn after_a_reload_to_caps_running_runners_count_at_their_cpu_limits() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &cpu_measured());
    engine.cpus = Some(4000);
    history(&handle.store, G, 0).await;
    // Each build used half a CPU over its run.
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .execute("UPDATE jobs SET cpu_average = 500", [])
        .unwrap();
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4, "4 × 0.5 CPU fit 4");

    let caps = cpu_measured().replace("admission = \"measured\"\n", "");
    engine.reload(Config::parse(&caps).unwrap(), None, Some(4000));
    engine.publish();
    // Each container's limit is the profile's 2 CPUs: 8 of the 4 the budget has.
    assert_eq!(
        handle.status.borrow().budget.cpu.map(|cpu| cpu.in_use),
        Some(8000)
    );
    github.0.lock().unwrap().queued = (1..=6).map(job).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        4,
        "no 2-CPU runner fits a budget the running ones fill"
    );
}

#[tokio::test]
async fn a_reload_that_sets_cpu_overcommit_admits_by_the_slices_cpus_it_read() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    // Started without a CPU budget, so the slice's CPUs were never read.
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.publish();
    assert_eq!(handle.status.borrow().budget.cpu, None);
    let with_budget = CONFIG.replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
    );
    engine.reload(Config::parse(&with_budget).unwrap(), None, Some(2000));
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    engine.tick(Shutdown::No).await;
    // Under caps each runner reserves the profile's 2 CPUs: one fits.
    assert_eq!(registered(&github.calls()).len(), 1);
    engine.publish();
    assert_eq!(
        handle.status.borrow().budget.cpu,
        Some(api::CpuBudget {
            cpus: 2000,
            budget: 2000,
            in_use: 2000,
            held: true,
        })
    );
}

/// How many runners of `repository` hold a slot.
fn runners_of(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    handle: &Handle,
    repository: &str,
) -> usize {
    engine.publish();
    handle
        .status
        .borrow()
        .profiles
        .iter()
        .flat_map(|profile| &profile.slots)
        .filter(|slot| {
            slot.runner
                .as_ref()
                .is_some_and(|runner| runner.repository == repository)
        })
        .count()
}

/// A queued `lint` job of o/b.
fn lint(id: u64) -> QueuedJob {
    QueuedJob {
        name: "lint".to_owned(),
        labels: vec!["self-hosted".to_owned(), "light".to_owned()],
        ..job(id)
    }
}

#[tokio::test]
async fn a_heavy_job_keeps_its_place_for_cpu_through_high_water_ticks_and_starts_once_the_cpu_drains()
 {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    // o/r's build uses 4 CPUs over its run, o/b's lint 1, on a slice of 4 CPUs.
    let config = format!(
        "{}\n[[repositories]]\nname = \"o/b\"\nprofile = \"app\"\nlabels = [\"self-hosted\", \"light\"]\nwarm = 0\n",
        cpu_measured()
            .replace("cpus = 2", "cpus = 4")
            .replace("slots = 6", "slots = 8")
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.memory_max = Some(10 * G);
    engine.cpus = Some(4000);
    history(&handle.store, G, 0).await;
    history_of(&handle.store, "lint", 2000, G, 0).await;
    let db = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    for statement in [
        "UPDATE jobs SET cpu_average = 4000 WHERE name = 'build'",
        "UPDATE jobs SET cpu_average = 1000, repository = 'o/b' WHERE name = 'lint'",
        "UPDATE runners SET repository = 'o/b' WHERE name LIKE 'earlier-2%'",
    ] {
        db.execute(statement, []).unwrap();
    }
    engine.recover().await.unwrap();
    slice_at(&handle.store, 2 * G);

    // Three lints start and take their jobs: 3 of the 4 CPUs.
    github.0.lock().unwrap().queued = (10..13).map(lint).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(runners_of(&mut engine, &handle, "o/b"), 3);
    {
        let mut state = github.0.lock().unwrap();
        state.queued.clear();
        for runner in &mut state.runners {
            runner.busy = true;
        }
    }
    // The build queues; it needs all 4 CPUs.
    github.0.lock().unwrap().queued = vec![job(20)];
    engine.tick(Shutdown::No).await;
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    assert!(
        budget.cpu.is_some_and(|cpu| cpu.held),
        "the build waits for CPU: {budget:?}"
    );
    // As if it had waited past the limit already.
    engine
        .repositories
        .get_mut("o/r")
        .and_then(|state| state.waits.get_mut("app"))
        .and_then(|waits| waits.cpu.as_mut())
        .expect("the build's wait is kept")
        .since -= PRIORITY_AFTER;

    // Every other tick the slice is at its high-water mark, and a lint queues every tick; one lint runner ends every
    // second round, so CPU keeps freeing up.
    let mut next = 30;
    for round in 0..6 {
        slice_at(&handle.store, 95 * G / 10);
        github.0.lock().unwrap().queued.push(lint(next));
        next += 1;
        engine.tick(Shutdown::No).await;
        slice_at(&handle.store, 2 * G);
        github.0.lock().unwrap().queued.push(lint(next));
        next += 1;
        if round % 2 == 1 {
            let mut state = docker.0.lock().unwrap();
            if let Some(at) = state.containers.iter().position(|container| {
                container
                    .labels
                    .get("runnerd.repository")
                    .map(String::as_str)
                    == Some("o/b")
            }) {
                state.containers.remove(at);
            }
        }
        engine.tick(Shutdown::No).await;
        if runners_of(&mut engine, &handle, "o/r") == 1 {
            break;
        }
        assert!(
            runners_of(&mut engine, &handle, "o/b") <= 3,
            "no younger lint takes the CPU the build waits for (round {round})"
        );
    }
    assert_eq!(
        runners_of(&mut engine, &handle, "o/r"),
        1,
        "the build started once the lints it waited for drained"
    );
    assert_eq!(runners_of(&mut engine, &handle, "o/b"), 0);
    assert_eq!(
        engine.repositories["o/r"].waits["app"],
        crate::scheduler::Waits {
            served: true,
            ..Default::default()
        },
        "started, the build no longer waits, and the barrier served its pool"
    );
}

#[tokio::test]
async fn a_job_held_for_cpu_that_leaves_the_queue_no_longer_waits() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let config = CONFIG.replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
    );
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    // Room for one runner at the profile's 2 CPUs.
    engine.cpus = Some(2000);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine.repositories["o/r"]
            .waits
            .get("app")
            .and_then(|waits| waits.cpu)
            .map(|wait| wait.job),
        Some(2)
    );
    // Its run is cancelled: GitHub lists it no more.
    github.0.lock().unwrap().queued = vec![job(1)];
    engine.tick(Shutdown::No).await;
    assert!(
        engine.repositories["o/r"].waits.is_empty(),
        "a job gone from the queue no longer waits"
    );
}

/// The host's memory read now: 64G with `available` of it, the slice's runners holding `slice` of it without page
/// cache, and its tasks stalled `stalled` hundredths of a percent of the last 10 s; a Gradle daemon and an emulator
/// hold the most of it outside the slice.
fn host_at(store: &Store, slice: u64, available: u64, stalled: u32) {
    store.host_memory(api::HostMemory {
        at: time::now_ms() / 1000,
        total: 64 * G,
        available,
        slice,
        some: Some(api::Pressure::default()),
        full: Some(api::Pressure {
            avg10: stalled,
            avg60: 0,
        }),
        holders: vec![
            api::Holder {
                name: "java".to_owned(),
                memory: 9 * G,
            },
            api::Holder {
                name: "qemu-system-x86".to_owned(),
                memory: 6 * G,
            },
        ],
        ceiling: None,
        swap: None,
    });
}

/// How long each of the jobs 1 to 4 waited, in ms, by the history's `column`: `host_waited` or `slot_waited`.
fn waited(dir: &TestDir, column: &str) -> Vec<u64> {
    let db = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    let mut statement = db
        .prepare(&format!(
            "SELECT {column} FROM jobs WHERE id <= 4 ORDER BY id"
        ))
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// What each queued job waits for, oldest first.
fn waits(handle: &Handle) -> Vec<Option<api::Waiting>> {
    handle
        .status
        .borrow()
        .queued
        .iter()
        .map(|job| job.waiting.clone())
        .collect()
}

#[tokio::test]
async fn jobs_wait_while_the_host_is_short_of_memory_and_start_once_it_has_room_again() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    slice_at(&handle.store, 0);
    // A 64G host keeps 4G free; past that it has room for two runners of 1.25G, which the slice would hold four of.
    host_at(&handle.store, 0, 4 * G + 2 * 1280 * M + 100 * M, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
    engine.publish();
    // Once short with runners running, the host keeps half the margin more free: nothing is to spare until 6G.
    let short = Some(api::Waiting::HostMemory {
        needs: 1280 * M,
        free: 0,
    });
    assert_eq!(
        waits(&handle)[2..],
        [short.clone(), short],
        "the two left wait for the host's memory"
    );
    // Insights are told the same two jobs, with the reading that says who holds the memory.
    handle.store.flush().await;
    let hold = handle.store.insights(0).await.unwrap().host.unwrap();
    assert_eq!(hold.jobs.len(), 2, "the two left wait: {hold:?}");
    assert_eq!(hold.memory.holders[0].name, "java");
    let reason = {
        let status = handle.status.borrow();
        let job = &status.queued[2];
        job.waiting.as_ref().unwrap().reason(job, &status, 0)
    };
    assert!(
        reason.starts_with("The host is short of memory"),
        "{reason}"
    );
    // The runners hold nothing yet, so all that is not available is held by the host's other programs, the
    // largest of which it names.
    let outside = crate::config::ByteSize(64 * G - (4 * G + 2 * 1280 * M + 100 * M));
    assert!(
        reason.ends_with(&format!(
            "programs outside CI hold {outside}, most java 9G and qemu-system-x86 6G"
        )),
        "{reason}"
    );

    // Room for one more by the 4G margin, none by the larger one: this plan holds it, and as it fits the plain margin
    // the next plan has that (ADR 0102).
    tokio::time::sleep(Duration::from_millis(20)).await;
    host_at(&handle.store, 0, 4 * G + 3 * 1280 * M + 100 * M, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2, "the larger margin");
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        3,
        "one fits the plain one"
    );
    // The last fits neither, so the larger margin stands again, until the host has room for it.
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 3, "no room is left");
    assert!(
        engine.measured.host.short,
        "no waiter fits the plain margin"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    host_at(&handle.store, 0, 6 * G + 4 * 1280 * M + 100 * M, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4);
    handle.store.flush().await;
    assert_eq!(
        handle.store.insights(0).await.unwrap().host,
        None,
        "once they start, no job waits for the host's memory"
    );
    let waited = waited(&dir, "host_waited");
    assert_eq!(waited[..2], [0, 0], "the jobs that started waited not");
    assert!(
        waited[2] >= 20 && waited[3] >= 40,
        "each that waited counts the ticks it waited: {waited:?}"
    );
    assert!(removed(&docker.calls()).is_empty(), "no runner was stopped");
}

#[tokio::test]
async fn jobs_count_a_wait_for_a_slot_only_while_memory_had_room_for_their_runners() {
    // Catches auto sizing adding slots for jobs memory would have held back anyway (ADR 0090). Two slots, four jobs
    // of 1.25G, and a slice with room for one more runner beside the two that start: job 3 waits for a slot alone,
    // job 4 for memory too.
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(
        &github,
        &docker,
        &dir,
        &measured().replace("slots = 4", "slots = 2"),
    );
    engine.memory_max = Some(3 * 1280 * M + 100 * M);
    history(&handle.store, G, 0).await;
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    slice_at(&handle.store, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
    tokio::time::sleep(Duration::from_millis(20)).await;
    engine.tick(Shutdown::No).await;
    handle.store.flush().await;
    let waited = waited(&dir, "slot_waited");
    assert_eq!(
        waited.iter().map(|&ms| ms >= 20).collect::<Vec<_>>(),
        [false, false, true, false],
        "only job 3 waited for a slot alone: {waited:?}"
    );
}

#[tokio::test]
async fn the_first_runner_into_an_empty_slice_needs_only_the_margin_after_a_wait() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    slice_at(&handle.store, 0);
    host_at(&handle.store, 0, 4 * G + 1000 * M, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 0, "1.25G does not fit");
    // The jobs waited, but nothing runs that a start would follow: the 4G margin alone holds.
    host_at(&handle.store, 0, 4 * G + 1280 * M + 100 * M, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
}

#[tokio::test]
async fn a_runner_using_more_than_it_reserved_takes_what_it_uses_from_the_host() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    github.0.lock().unwrap().queued = vec![job(1)];
    slice_at(&handle.store, 0);
    host_at(&handle.store, 0, 40 * G, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    // It reserved 1.25G and holds 3G, which the host no longer has available: what is left beside it is
    // MemAvailable less the 4G margin, room for one more 1.25G runner, not two.
    runners_use(&github, &handle.store, 3 * G);
    slice_at(&handle.store, 3 * G);
    host_at(&handle.store, 3 * G, 4 * G + 1280 * M + 100 * M, 0);
    github.0.lock().unwrap().queued = (1..=3).map(job).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
}

#[tokio::test]
async fn under_caps_a_runner_starts_only_if_its_cap_fits_what_the_host_leaves() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.memory_max = Some(20 * G);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    // The slice fits both 4G caps; the host past its 4G margin fits one.
    host_at(&handle.store, 0, 8 * G + 100 * M, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    engine.publish();
    assert_eq!(
        waits(&handle)[1],
        Some(api::Waiting::HostMemory {
            needs: 4 * G,
            free: 0
        })
    );
}

/// A VM's memory read now, as Hyper-V leaves it while idle: 2.12G of which 1.48G is available, the slice holding
/// nothing, its tasks stalled `stalled` hundredths of a percent of the last 10 s.
fn idle_vm_at(store: &Store, stalled: u32) {
    store.host_memory(api::HostMemory {
        at: time::now_ms() / 1000,
        total: 2 * G + 120 * M,
        available: G + 490 * M,
        slice: 0,
        some: Some(api::Pressure::default()),
        full: Some(api::Pressure {
            avg10: stalled,
            avg60: 0,
        }),
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    });
}

/// An engine under caps with a 20G slice, its config's `[host]` extended by `host`, two jobs queued.
async fn vm_engine(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    host: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let config = CONFIG.replace(
        "isolation = \"sysbox\"",
        &format!("isolation = \"sysbox\"\n{host}"),
    );
    let (mut engine, handle) = engine_with(github, docker, dir, &config);
    engine.memory_max = Some(20 * G);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    (engine, handle)
}

#[tokio::test]
async fn an_idle_vm_without_a_memory_ceiling_starts_nothing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = vm_engine(&github, &docker, &dir, "").await;
    idle_vm_at(&handle.store, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        0,
        "1.48G is under the 2G margin"
    );
    engine.publish();
    assert_eq!(
        waits(&handle)[0],
        Some(api::Waiting::HostMemory {
            needs: 4 * G,
            free: 0
        })
    );
}

#[tokio::test]
async fn a_vm_that_can_grow_to_its_memory_ceiling_starts_runners_in_what_it_can_grow_by() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = vm_engine(&github, &docker, &dir, "memory_ceiling = \"24G\"").await;
    idle_vm_at(&handle.store, 0);
    engine.tick(Shutdown::No).await;
    // 1.48G available and 21.88G to grow by, less the 2G margin of 24G, fits both 4G caps.
    assert_eq!(registered(&github.calls()).len(), 2);
    engine.publish();
    let line = handle.status.borrow().budget.host_line().unwrap();
    assert!(
        line.starts_with("host: 1.48G of 2.12G available, growing to 24G; leaves the slice 21.36G"),
        "{line}"
    );
}

#[tokio::test]
async fn a_vm_that_stalls_short_of_memory_starts_nothing_whatever_its_memory_ceiling() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = vm_engine(&github, &docker, &dir, "memory_ceiling = \"24G\"").await;
    // The hypervisor does not give what the ceiling promises: every runnable task waits for memory 6% of the time.
    idle_vm_at(&handle.store, 600);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 0, "nothing starts");
    engine.publish();
    let status = handle.status.borrow();
    assert!(
        status.budget.host.as_ref().is_some_and(|host| host.stalled),
        "the budget says the host stalls"
    );
    let job = &status.queued[0];
    let reason = job.waiting.as_ref().unwrap().reason(job, &status, 0);
    assert!(reason.contains("stall"), "{reason}");
}

#[tokio::test]
async fn a_runner_whose_room_the_host_lost_since_the_plan_does_not_start_in_the_same_tick() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.memory_max = Some(20 * G);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=2).map(job).collect();
    // The plan has room for both 4G caps; while the first runner starts, other programs take the host's memory
    // down to what it alone fits in.
    host_at(&handle.store, 0, 20 * G, 0);
    let store = handle.store.clone();
    docker.0.lock().unwrap().on_start = Some(Box::new(move |_| {
        host_at(&store, 0, 8 * G + 100 * M, 0);
    }));
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "the second start waits for room"
    );
    engine.publish();
    assert_eq!(
        waits(&handle)[1],
        Some(api::Waiting::HostMemory {
            needs: 4 * G,
            free: 0
        })
    );
}

#[tokio::test]
async fn nothing_starts_while_the_host_stalls_short_of_memory() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    slice_at(&handle.store, 0);
    // 7.5G available is under twice the 4G margin, and every runnable task waited for memory 6% of the last 10 s:
    // nothing starts, though even the larger margin leaves room for one. The runners hold 10G of the rest.
    host_at(&handle.store, 10 * G, 7 * G + 512 * M, 600);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 0, "nothing starts");
    engine.publish();
    {
        let status = handle.status.borrow();
        assert!(
            status.budget.host.as_ref().is_some_and(|host| host.stalled),
            "the budget says the host stalls"
        );
        let job = status.queued.last().unwrap();
        let reason = job.waiting.as_ref().unwrap().reason(job, &status, 0);
        assert!(reason.contains("stall"), "{reason}");
        // 64G less the 7.5G available and the runners' 10G: the runners' memory is not the other programs'.
        let outside = crate::config::ByteSize(64 * G - 7 * G - 512 * M - 10 * G);
        assert!(
            reason.ends_with(&format!(
                "programs outside CI hold {outside}, most java 9G and qemu-system-x86 6G"
            )),
            "{reason}"
        );
    }
    // 3%: under where starts stop, over where they resume.
    host_at(&handle.store, 0, 7 * G + 512 * M, 300);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 0, "still held at 3%");
    host_at(&handle.store, 0, 7 * G + 512 * M, 50);
    engine.tick(Shutdown::No).await;
    // 3.5G past the margin: two 1.25G runners.
    assert_eq!(registered(&github.calls()).len(), 2, "resumed under 1%");
}

#[tokio::test]
async fn a_stall_with_memory_to_spare_holds_no_start_and_stops_no_runner() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    slice_at(&handle.store, 0);
    github.0.lock().unwrap().queued = vec![job(1)];
    host_at(&handle.store, 0, 40 * G, 0);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    // 6% with 40G available: runners reclaiming at their own caps, not a host short of memory.
    github.0.lock().unwrap().queued = (1..=3).map(job).collect();
    host_at(&handle.store, 0, 40 * G, 600);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        3,
        "the stall holds nothing"
    );
    engine.publish();
    assert!(
        handle
            .status
            .borrow()
            .budget
            .host
            .as_ref()
            .is_some_and(|host| !host.stalled),
        "the budget says the host does not stall"
    );
    assert!(removed(&docker.calls()).is_empty(), "no runner was stopped");
}

#[tokio::test]
async fn without_a_recent_reading_of_the_host_only_the_slice_bounds_starts() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    slice_at(&handle.store, 0);
    handle.store.host_memory(api::HostMemory {
        at: time::now_ms() / 1000 - 60,
        total: 64 * G,
        available: 0,
        slice: 0,
        some: None,
        full: None,
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    });
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4);
}

#[tokio::test]
async fn runners_a_restart_adopts_keep_the_cpu_they_reserved_not_their_limits() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &cpu_measured());
    engine.cpus = Some(4000);
    history(&handle.store, G, 0).await;
    // Each build used half a CPU over its run.
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .execute("UPDATE jobs SET cpu_average = 500", [])
        .unwrap();
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = (1..=4).map(job).collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4, "4 × 0.5 CPU fit 4");
    handle.store.flush().await;
    drop((engine, handle));

    // The daemon restarts while the four run: each still reserves half a CPU, not its 2-CPU limit.
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &cpu_measured());
    engine.cpus = Some(4000);
    engine.recover().await.unwrap();
    engine.publish();
    assert_eq!(
        handle.status.borrow().budget.cpu.map(|cpu| cpu.in_use),
        Some(2000)
    );
}

#[tokio::test]
async fn a_start_that_no_longer_fits_does_not_hold_back_the_plans_later_starts() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let config = sized("0G", "4G", "512M").replacen("slots = 1", "slots = 2", 1);
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
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
    // The plan has room for all three; while the first runner starts, other programs take the host's memory down to
    // what it and a 512M runner fit in, not a second 4G one.
    host_at(&handle.store, 0, 20 * G, 0);
    let store = handle.store.clone();
    docker.0.lock().unwrap().on_start = Some(Box::new(move |_| {
        host_at(&store, 0, 9 * G + 100 * M, 0);
    }));
    engine.tick(Shutdown::No).await;
    // Skipping every start after the one that no longer fit left o/s's job to the next poll, though it fit.
    let registered: Vec<String> = registered(&github.calls()).into_iter().cloned().collect();
    assert_eq!(registered.len(), 2, "{registered:?}");
    assert!(
        registered[1].contains("-small-"),
        "o/s's runner starts in the same tick: {registered:?}"
    );
    engine.publish();
    assert_eq!(
        waits(&handle)[1],
        Some(api::Waiting::HostMemory {
            needs: 4 * G,
            free: 0
        })
    );
}

#[tokio::test]
async fn a_stop_asked_for_while_a_runner_starts_starts_no_more_of_the_tick() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let config = sized("0G", "4G", "512M").replacen("slots = 1", "slots = 2", 1);
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
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
    host_at(&handle.store, 0, 20 * G, 0);
    // While the first runner starts, the host's memory goes down, which defers the second, and a stop is asked for.
    let (store, control) = (handle.store.clone(), handle.control.clone());
    docker.0.lock().unwrap().on_start = Some(Box::new(move |_| {
        host_at(&store, 0, 9 * G + 100 * M, 0);
        control.shutdown(Shutdown::Detach);
    }));
    engine.tick(Shutdown::No).await;
    // A runner started after the stop could take a job the detach then leaves running.
    assert_eq!(registered(&github.calls()).len(), 1, "{:?}", github.calls());
    let started = docker
        .calls()
        .iter()
        .filter(|call| call.starts_with("start "))
        .count();
    assert_eq!(started, 1, "{:?}", docker.calls());
}

/// A pass that runs while a tick follows its jobs plans after the tick took a new listing but before it raised
/// what waiting runners reserve for it: it must raise them first. The bug it catches: a pass admitting runners
/// beside a waiting runner still counted at its old, smaller reservation, past the slice once it is raised.
#[tokio::test]
async fn a_pass_raises_what_waiting_runners_reserve_before_it_admits() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    github.0.lock().unwrap().queued = vec![job(1), job(2)];
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2, "1.25G for each build");
    // One runner ran build 2, which ended; the tick that follows has listed two e2e jobs beside build 1 and not yet
    // raised the waiting runner's reservation for them when Docker says the other's container stopped.
    let ended = {
        let occupant = engine
            .slots
            .values_mut()
            .flatten()
            .flatten()
            .last()
            .unwrap();
        occupant.job = Some(2);
        occupant.busy = true;
        occupant.container.clone()
    };
    for container in &mut docker.0.lock().unwrap().containers {
        if container.id == ended {
            container.running = false;
        }
    }
    let e2e = |id| QueuedJob {
        name: "e2e".to_owned(),
        ..job(id)
    };
    engine.repositories.get_mut("o/r").unwrap().queued = vec![job(1), e2e(5), e2e(6)];
    engine.exits.stopped(ended);
    engine.turnover().await;

    engine.rereserve().await;
    engine.publish();
    let in_use = handle.status.borrow().budget.memory_in_use;
    assert!(
        in_use <= 10 * G,
        "the runners reserve {in_use} bytes of the 10G slice"
    );
}

/// Four builds run at 1.25G each. One ends while the tick that listed an e2e beside the other three has not raised
/// their reservations yet: the pass that releases it raises them to 3.5G each, 10.5G of the 10G slice, and stops
/// none, as passes stop no runner. The bug it catches: the next tick finding nothing left to raise and forgetting
/// that the three were raised, so none stopped and they stayed past the budget.
#[tokio::test]
async fn waiting_runners_a_pass_raised_past_the_budget_are_stopped_by_the_next_tick() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = measured_engine(&github, &docker, &dir, G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    slice_at(&handle.store, 2 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4, "4 × 1.25G");
    let ended = {
        let occupant = engine
            .slots
            .values_mut()
            .flatten()
            .flatten()
            .last()
            .unwrap();
        occupant.job = Some(4);
        occupant.busy = true;
        occupant.container.clone()
    };
    for container in &mut docker.0.lock().unwrap().containers {
        if container.id == ended {
            container.running = false;
        }
    }
    let e2e = QueuedJob {
        name: "e2e".to_owned(),
        ..job(5)
    };
    let queued = vec![job(1), job(2), job(3), e2e];
    engine.repositories.get_mut("o/r").unwrap().queued = queued.clone();
    github.0.lock().unwrap().queued = queued;
    engine.exits.stopped(ended);
    engine.turnover().await;
    engine.publish();
    assert_eq!(
        handle.status.borrow().budget.memory_in_use,
        3 * 3584 * M,
        "the pass raised the three left"
    );

    engine.tick(Shutdown::No).await;
    engine.publish();
    let in_use = handle.status.borrow().budget.memory_in_use;
    assert!(
        in_use <= 10 * G,
        "the runners reserve {in_use} bytes of the 10G slice"
    );
}

/// `measured()` with the pooled count off.
fn unpooled() -> String {
    measured().replace(
        "admission = \"measured\"",
        "admission = \"measured\"\npooled_count = false",
    )
}

/// A recent sample of every runner using `used`, and of a slice and a host with room to spare.
fn roomy_readings(github: &FakeGithub, store: &Store, used: u64) {
    runners_use(github, store, used);
    slice_at(store, G);
    host_at(store, G, 40 * G, 0);
}

/// Every running job's first minute went by.
fn first_minute_passes(engine: &mut Engine<FakeGithub, FakeDocker>) {
    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.job_started = occupant.job_started.map(|at| at - POOL_AFTER);
    }
}

/// An 8G slice running `build` and `e2e`, each predicted at 3.5G and using 512M, 1G left of the plain count: their
/// runners' room above use is 3G each, which comes to 1.76G of credit.
pub(super) async fn two_settled_jobs(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let (mut engine, handle) = engine_with(github, docker, dir, config);
    engine.memory_max = Some(8 * G);
    history_of(&handle.store, "build", 1000, 3 * G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    history_of(&handle.store, "lint", 3000, G, 0).await;
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![
        job(1),
        QueuedJob {
            name: "e2e".to_owned(),
            ..job(2)
        },
    ];
    for slot in 1..=2 {
        let capture = format!("@@job {slot} {slot}\n");
        docker.0.lock().unwrap().slot_captures.insert(slot, capture);
    }
    roomy_readings(github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
    until_state(&handle, 1, JobState::Running).await;
    until_state(&handle, 2, JobState::Running).await;
    github.0.lock().unwrap().queued = Vec::new();
    roomy_readings(github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    first_minute_passes(&mut engine);
    roomy_readings(github, &handle.store, 512 * M);
    (engine, handle)
}

/// A queued job of o/r predicted at 1.25G.
fn small(id: u64) -> QueuedJob {
    QueuedJob {
        name: "lint".to_owned(),
        ..job(id)
    }
}

/// A 16G host with `available` of it, the slice's runners holding 1G: it keeps a 2G margin, so at 8G it leaves the
/// slice 7G and a credit can stand.
fn small_host_at(store: &Store, available: u64) {
    store.host_memory(api::HostMemory {
        at: time::now_ms() / 1000,
        total: 16 * G,
        available,
        slice: G,
        some: Some(api::Pressure::default()),
        full: Some(api::Pressure::default()),
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    });
}

/// What the only queued job's wait says, as every client shows it.
fn only_reason(handle: &Handle) -> String {
    let status = handle.status.borrow();
    let job = &status.queued[0];
    job.waiting.as_ref().unwrap().reason(job, &status, 0)
}

/// The kinds of insight the history gives now.
async fn host_held_insights(engine: &Engine<FakeGithub, FakeDocker>, handle: &Handle) -> usize {
    handle.store.flush().await;
    let history = handle.store.insights(0).await.unwrap();
    crate::insights::insights(&history, &engine.config, 7, time::now_ms())
        .insights
        .iter()
        .filter(|insight| matches!(insight.kind, api::InsightKind::HostHeld { .. }))
        .count()
}

#[tokio::test]
async fn a_job_the_host_limit_holds_with_room_free_asks_nobody_to_close_programs() {
    // Catches the owner being told to close programs while the host has the room, because what runs reserves more
    // than it holds and the credit opened the slice to one more job than the host limit lets in.
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    github.0.lock().unwrap().queued = vec![small(3)];
    runners_use(&github, &handle.store, 512 * M);
    slice_at(&handle.store, G);
    small_host_at(&handle.store, 8 * G);
    engine.tick(Shutdown::No).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    runners_use(&github, &handle.store, 512 * M);
    slice_at(&handle.store, G);
    small_host_at(&handle.store, 8 * G);
    engine.tick(Shutdown::No).await;
    engine.publish();

    assert!(
        handle.status.borrow().budget.pooled.is_some(),
        "a credit opens the slice"
    );
    assert_eq!(
        waits(&handle),
        [Some(api::Waiting::HostMemory {
            needs: 1280 * M,
            free: 0
        })],
        "the host limit reads the plain count, so it holds the job"
    );
    assert_eq!(
        only_reason(&handle),
        "The host has 8G of 16G available, but what runs may still grow into it: needs 1.25G; 0B of the 6G it leaves \
         the slice is free"
    );
    // Sizing takes this wait off the one it reads as a wait for the budget, so it has to be counted.
    handle.store.flush().await;
    assert!(
        waited(&dir, "host_waited")[2] >= 20,
        "the job waited for the host's limit"
    );
    handle.store.host_waited(vec![3], 60 * 60_000);
    assert_eq!(
        host_held_insights(&engine, &handle).await,
        0,
        "6G of room past the margin, and the job needs 1.25G"
    );

    // Programs take the room: the same job now waits for a host that lacks it. That takes the credit back, and a job
    // short of both budgets reads the slice's, so the slice gets room of its own.
    engine.memory_max = Some(9 * G);
    runners_use(&github, &handle.store, 512 * M);
    slice_at(&handle.store, G);
    small_host_at(&handle.store, 3 * G);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert!(
        only_reason(&handle).starts_with("The host is short of memory: needs 1.25G"),
        "{}",
        only_reason(&handle)
    );
    assert_eq!(
        host_held_insights(&engine, &handle).await,
        1,
        "1G past the margin, and the job needs 1.25G"
    );

    // The job would fit, but only by taking what the host is about to run out of: closing programs is what starts it.
    runners_use(&github, &handle.store, 512 * M);
    slice_at(&handle.store, G);
    small_host_at(&handle.store, 3 * G + 512 * M);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert!(
        only_reason(&handle).starts_with("The host is short of memory: needs 1.25G"),
        "{}",
        only_reason(&handle)
    );
    assert_eq!(
        host_held_insights(&engine, &handle).await,
        1,
        "1.5G past the margin, under one more margin of 2G"
    );

    // Room comes back: the next plan takes the job off what the owner is told.
    runners_use(&github, &handle.store, 512 * M);
    slice_at(&handle.store, G);
    small_host_at(&handle.store, 8 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(host_held_insights(&engine, &handle).await, 0);
}

#[tokio::test]
async fn settled_jobs_of_two_kinds_make_room_a_start_goes_by_and_every_reader_agrees() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    let pooled = budget.pooled.expect("two settled jobs, room above use");
    assert_eq!(budget.memory_in_use, 7 * G, "the plain count stays a fact");
    assert_eq!(pooled.counted, 7 * G - pooled.credit);
    assert_eq!(pooled.runners, 2);
    // 6G less the root of 2 × 3G².
    assert_eq!(pooled.credit / M, 1799);
    let remembered = engine.host_remembered();
    assert_eq!(engine.count(remembered).pooled(), pooled.counted);
    let plan = engine.plan_budget(&engine.admitting(&mut [], None));
    assert_eq!(
        plan.memory,
        Some(crate::scheduler::left(8 * G, pooled.counted)),
        "the plan starts from what the status shows"
    );
    assert_eq!(
        budget.host.map(|host| host.spare),
        Some(G + 40 * G - 4 * G - budget.memory_in_use),
        "two jobs keep all their headroom for growth: the host's limit reads the plain count"
    );
    assert_eq!(
        plan.host,
        Some(crate::scheduler::left(
            G + 40 * G - 4 * G,
            budget.memory_in_use
        )),
        "and so does a plan's"
    );

    // 1G is left of the plain count: lint's 1.25G fits only in the pooled one.
    github.0.lock().unwrap().queued = vec![small(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 3, "lint starts");
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    assert_eq!(budget.memory_in_use, 8 * G + 256 * M, "past 8G, in full");
    assert_eq!(
        budget.pooled.map(|after| after.credit),
        Some(pooled.credit),
        "a runner just started changes no credit"
    );

    // The next tick does not stop the runner the last one started.
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    let stopped = github
        .calls()
        .iter()
        .filter(|call| call.starts_with("delete "))
        .count();
    assert_eq!(stopped, 0, "what fits the pooled count stays");
}

#[tokio::test]
async fn a_raised_waiting_runner_stops_only_once_the_pooled_count_is_past_the_budget() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    github.0.lock().unwrap().queued = vec![small(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 3, "lint's runner waits");
    github.0.lock().unwrap().queued = Vec::new();
    let waiting = engine
        .occupied()
        .find(|(_, occupant)| occupant.job.is_none())
        .map(|(_, occupant)| occupant.runner_name.clone())
        .unwrap();
    engine.measured.raised.push(waiting);

    // 8.25G plain of the 8G budget: past it, but the count is not.
    roomy_readings(&github, &handle.store, 512 * M);
    assert_eq!(engine.over_budget(&[]), [], "the pooled count fits");

    // The host's reading goes old, the credit with it: the same runner is now past the budget.
    handle.store.host_memory(api::HostMemory {
        at: time::now_ms() / 1000 - 60,
        total: 64 * G,
        available: 40 * G,
        slice: G,
        some: None,
        full: None,
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    });
    assert_eq!(engine.over_budget(&[]).len(), 1, "the plain count does not");
}

#[tokio::test]
async fn the_last_check_before_a_start_keeps_the_margin_its_plan_chose() {
    // The runners a plan starts into an empty slice need only the first runner's margin, and the credit's host guard
    // goes by that margin: read afresh, the larger one takes the credit back from a start the plan allowed.
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    // 21G available: 1G past the 20G kept by the margin, under the 22G kept by the larger one.
    host_at(&handle.store, G, 21 * G, 0);
    engine.measured.host.short = true;
    let lint = 1280 * M;
    let mut plan = engine.admitting(&mut [], None);
    plan.remembered.short = false;
    assert!(
        engine.still_fits(&plan, "app", lint, 0),
        "1G left and 1G of credit, by the plan's margin"
    );
    plan.remembered.short = true;
    assert!(
        !engine.still_fits(&plan, "app", lint, 0),
        "no credit by the larger margin"
    );
}

#[tokio::test]
async fn a_reseed_reads_the_room_a_plan_reads() {
    // A plan fills what the pooled count leaves; a reseed reading the plain count would wait on room plans keep taking.
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let second_profile = |config: String| {
        config.replace(
            "[[repositories]]",
            "[profiles.spare]\nimage_stage = \"spare\"\ncpus = 2\nmemory = \"2G\"\nslots = 1\n\n\
             [[repositories]]\nname = \"o/s\"\nprofile = \"spare\"\nlabels = [\"spare\"]\nwarm = 0\n\n\
             [[repositories]]",
        )
    };
    let (engine, _) = two_settled_jobs(&github, &docker, &dir, &second_profile(measured())).await;
    assert!(
        engine.fits_seed("spare"),
        "2G of the 2.76G the count leaves"
    );

    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (engine, _) = two_settled_jobs(&github, &docker, &dir, &second_profile(unpooled())).await;
    assert!(!engine.fits_seed("spare"), "1G left of the plain count");
}

#[tokio::test]
async fn with_the_pooled_count_off_the_same_jobs_count_and_plan_as_they_add_up() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &unpooled()).await;
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    assert_eq!(budget.pooled, None);
    assert_eq!(budget.memory_in_use, 7 * G);
    let plan = engine.plan_budget(&engine.admitting(&mut [], None));
    assert_eq!(plan.memory, Some(crate::scheduler::left(8 * G, 7 * G)));
    assert_eq!(plan.credit_room, 0, "nothing for a barrier to expect");
    github.0.lock().unwrap().queued = vec![small(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2, "lint's 1.25G waits");
}

#[tokio::test]
async fn caps_admission_pools_nothing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    engine.config.host.admission = crate::config::Admission::Caps;
    engine.publish();
    assert_eq!(handle.status.borrow().budget.pooled, None);
    assert_eq!(handle.status.borrow().budget.memory_in_use, 8 * G);
}

#[tokio::test]
async fn any_stale_reading_takes_the_credit_back() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    let old = time::now_ms() - 60_000;
    let credit = |engine: &mut Engine<FakeGithub, FakeDocker>| {
        engine.publish();
        let budget = handle.status.borrow().budget.clone();
        budget.pooled.map(|pooled| pooled.credit)
    };
    assert!(credit(&mut engine).is_some(), "the readings are fresh");

    // The host's reading stops: the sampler reads /proc separately from the slice.
    handle.store.host_memory(api::HostMemory {
        at: old / 1000,
        total: 64 * G,
        available: 40 * G,
        slice: G,
        some: Some(api::Pressure::default()),
        full: Some(api::Pressure::default()),
        holders: Vec::new(),
        ceiling: None,
        swap: None,
    });
    assert_eq!(credit(&mut engine), None, "without a recent host reading");

    // The slice's sample goes old: every runner counts at its cap too.
    host_at(&handle.store, G, 40 * G, 0);
    handle.store.host_sample(api::HostSample {
        at: old / 1000,
        cpu: 0,
        memory: G,
    });
    assert_eq!(credit(&mut engine), None, "without a recent slice sample");
    assert_eq!(handle.status.borrow().budget.memory_in_use, 8 * G);

    // A runner's own sample goes old: it counts at its cap, and the other alone pools with nothing.
    slice_at(&handle.store, G);
    let stale = engine
        .occupied()
        .map(|(_, occupant)| occupant.runner_name.clone())
        .next()
        .unwrap();
    runners_use(&github, &handle.store, 512 * M);
    handle.store.samples(vec![SampleRecord {
        runner: stale.clone(),
        at: old,
        cpu: 0,
        cpu_usage: 0,
        memory: 512 * M,
        network_received: 0,
        network_sent: 0,
        cpu_held: crate::observe::metrics::CpuHeld::default(),
        events: Events::default(),
    }]);
    assert_eq!(credit(&mut engine), None, "its one job left to pool");
}

#[tokio::test]
async fn a_runner_a_restart_adopts_counts_in_full_for_the_rest_of_its_job() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    handle.store.flush().await;
    drop((engine, handle));

    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(8 * G);
    engine.recover().await.unwrap();
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    first_minute_passes(&mut engine);
    roomy_readings(&github, &handle.store, 512 * M);
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    assert_eq!(budget.memory_in_use, 7 * G, "both keep what they reserved");
    assert_eq!(budget.pooled, None, "this daemon did not see them start");
}

#[tokio::test]
async fn a_waiting_runner_a_restart_adopts_counts_in_full_once_it_takes_a_job() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(8 * G);
    history_of(&handle.store, "build", 1000, 3 * G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = vec![
        job(1),
        QueuedJob {
            name: "e2e".to_owned(),
            ..job(2)
        },
    ];
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
    handle.store.flush().await;
    // The last capture started may not have run yet, and would read the job as this daemon's.
    until_captures_end(&engine).await;
    drop((engine, handle));

    // Each takes its job while no daemon runs.
    for slot in 1..=2 {
        let capture = format!("@@job {slot} {slot}\n");
        docker.0.lock().unwrap().slot_captures.insert(slot, capture);
    }
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(8 * G);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    until_state(&handle, 1, JobState::Running).await;
    until_state(&handle, 2, JobState::Running).await;
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    first_minute_passes(&mut engine);
    roomy_readings(&github, &handle.store, 512 * M);
    let found = engine
        .occupied()
        .filter(|(_, occupant)| occupant.job_started.is_some() && occupant.key.is_some())
        .count();
    assert_eq!(found, 2, "this daemon found both jobs");
    engine.publish();
    assert_eq!(
        handle.status.borrow().budget.pooled,
        None,
        "their runners are not known to have started as predicted"
    );
}

#[tokio::test]
async fn a_host_that_stalls_or_runs_short_takes_the_credit_back_and_holds_the_start() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = two_settled_jobs(&github, &docker, &dir, &measured()).await;
    // Under the margin and a quarter of the host: 20G.
    host_at(&handle.store, G, 19 * G, 0);
    engine.publish();
    assert_eq!(handle.status.borrow().budget.pooled, None);
    github.0.lock().unwrap().queued = vec![small(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2, "lint waits again");
}

/// A 32G slice running `build`, `e2e` and `it`, each predicted at 3.5G and using 512M: 9G of headroom, of which the
/// host keeps the two largest, 6G, so its limit counts 3G less than the 10.5G reserved (ADR 0108).
async fn three_settled_jobs(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let (mut engine, handle) = engine_with(github, docker, dir, config);
    engine.memory_max = Some(32 * G);
    history_of(&handle.store, "build", 1000, 3 * G, 0).await;
    history_of(&handle.store, "e2e", 2000, 3 * G, 0).await;
    history_of(&handle.store, "it", 3000, 3 * G, 0).await;
    history_of(&handle.store, "lint", 4000, G, 0).await;
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = ["build", "e2e", "it"]
        .into_iter()
        .zip(1..)
        .map(|(name, id)| QueuedJob {
            name: name.to_owned(),
            ..job(id)
        })
        .collect();
    for slot in 1..=3 {
        let capture = format!("@@job {slot} {slot}\n");
        docker.0.lock().unwrap().slot_captures.insert(slot, capture);
    }
    roomy_readings(github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 3);
    for id in 1..=3 {
        until_state(&handle, id, JobState::Running).await;
    }
    github.0.lock().unwrap().queued = Vec::new();
    roomy_readings(github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    first_minute_passes(&mut engine);
    roomy_readings(github, &handle.store, 512 * M);
    (engine, handle)
}

/// The host reading `available` of 64G beside a slice holding 1G: it leaves the slice `available` less 3G.
fn tight_host(github: &FakeGithub, store: &Store, available: u64) {
    runners_use(github, store, 512 * M);
    slice_at(store, G);
    host_at(store, G, available, 0);
}

#[tokio::test]
async fn a_host_credit_starts_what_only_it_fits_and_a_restart_or_pooled_host_off_holds_it() {
    // Catches the host's limit reading the plain count where ADR 0108 has it read the host's credit, the credit
    // surviving `pooled_host = false`, and a restart counting runners it adopted at their use: then nothing may start
    // until the plain count fits (challenge finding 6).
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = three_settled_jobs(&github, &docker, &dir, &measured()).await;
    // 12G available leaves the slice 9G: 1.5G past the 7.5G counted, 1.5G short of the 10.5G reserved.
    tight_host(&github, &handle.store, 12 * G);
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    let host = budget.host.clone().expect("a fresh reading");
    assert_eq!(budget.memory_in_use, 10 * G + 512 * M);
    assert_eq!(
        host.credit,
        Some(api::HostCredit {
            bytes: 3 * G,
            used: G + 512 * M,
        })
    );
    assert_eq!(host.spare, G + 512 * M);
    assert!(
        budget
            .host_line()
            .unwrap()
            .contains("; what runs counts 7.5G: it uses 1.5G and keeps 6G for growth"),
        "{}",
        budget.host_line().unwrap()
    );
    let plan = engine.plan_budget(&engine.admitting(&mut [], None));
    assert_eq!(
        plan.host,
        Some(crate::scheduler::left(9 * G, 7 * G + 512 * M))
    );
    assert_eq!(
        plan.drained.host,
        Some(crate::scheduler::left(G + 12 * G - 6 * G, 0)),
        "a drain reads the larger margin, nothing of what runs and none of the credit"
    );

    github.0.lock().unwrap().queued = vec![small(4)];
    tight_host(&github, &handle.store, 12 * G);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        4,
        "lint starts on the host's credit"
    );

    // With `pooled_host = false` the same jobs leave lint waiting for the host.
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let off = measured().replace(
        "admission = \"measured\"",
        "admission = \"measured\"\npooled_host = false",
    );
    let (mut engine, handle) = three_settled_jobs(&github, &docker, &dir, &off).await;
    github.0.lock().unwrap().queued = vec![small(4)];
    tight_host(&github, &handle.store, 12 * G);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(registered(&github.calls()).len(), 3, "off, lint waits");
    assert_eq!(
        handle.status.borrow().budget.host.as_ref().unwrap().credit,
        None
    );

    // A restart adopts the three: none is known to have started as predicted, so the credit is gone and lint waits
    // until the plain count fits, though no runner of its own reached a minute.
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (engine, handle) = three_settled_jobs(&github, &docker, &dir, &measured()).await;
    handle.store.flush().await;
    drop((engine, handle));
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(32 * G);
    engine.recover().await.unwrap();
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    first_minute_passes(&mut engine);
    github.0.lock().unwrap().queued = vec![small(4)];
    tight_host(&github, &handle.store, 12 * G);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(
        registered(&github.calls()).len(),
        3,
        "after a restart, lint waits"
    );
    assert_eq!(
        handle.status.borrow().budget.host.as_ref().unwrap().credit,
        None
    );
    // 16G leaves 13G by the plain margin: lint fits beside the 10.5G reserved, and the next plan starts it (ADR 0102).
    for _ in 0..2 {
        tight_host(&github, &handle.store, 16 * G);
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(
        registered(&github.calls()).len(),
        4,
        "once the plain count fits"
    );
}

#[tokio::test]
async fn a_job_held_with_a_host_credit_waits_for_the_growth_it_keeps() {
    // Catches the wait of a job the host's limit holds with room free saying reservations where ADR 0108 counts use
    // and kept growth.
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine"),
    );
    let (mut engine, handle) = three_settled_jobs(&github, &docker, &dir, &measured()).await;
    github.0.lock().unwrap().queued = vec![small(4)];
    // 10G available leaves the slice 7G, 0.5G short of the 7.5G counted.
    tight_host(&github, &handle.store, 10 * G);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(registered(&github.calls()).len(), 3);
    assert_eq!(
        only_reason(&handle),
        "The host has 10G of 64G available, but what runs may still grow into it: it uses 1.5G and keeps 6G for \
         growth; needs 1.25G, 0B is free"
    );
}
