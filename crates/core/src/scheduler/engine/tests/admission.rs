//! Measured admission on the fakes: more runners than the caps fit when the history predicts small peaks, nothing
//! while the slice is at the high-water mark, the caps without a recent slice sample or after a kill for memory,
//! and the decision in each runner's record.

use super::*;
use crate::api::{AdmissionPolicy, Basis, JobPeak};
use crate::observe::metrics::Events;
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
            events: Events {
                oom: Some(0),
                oom_kill: Some(if i == count - 1 { oom_kills } else { 0 }),
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
    let reserved: Vec<String> = rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .prepare("SELECT admission FROM runners WHERE name NOT LIKE 'earlier-%'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
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
        // One build queues per poll; the runners before took theirs and warm up at 300M, sampled just now.
        {
            let mut state = github.0.lock().unwrap();
            for runner in &mut state.runners {
                (runner.status, runner.busy) = ("online".to_owned(), true);
            }
            state.queued = vec![job(copy)];
        }
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
