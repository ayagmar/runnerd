//! What a job's runner container wrote to disk (ADR 0143), as the history works it out from samples of its cgroup's
//! counters: the job's share of a warm runner, restarts, a counter that starts again, an end no daemon saw, and the
//! drive's windows insights read.

use rusqlite::Connection;

use super::{queued, runner, user_version};
use crate::api::{self, Measure};
use crate::config::History;
use crate::observe::disk::{Window, Written};
use crate::observe::metrics::{CpuHeld, Events};
use crate::store::{SampleRecord, Store, VERSION};
use crate::testdir::TestDir;

/// The schema at version 23, before disk writes.
const SCHEMA_23: &str = include_str!("schema-23.sql");

const MIB: u64 = 1 << 20;

/// A sample of `runner` at `at` (ms) whose cgroup had written `bytes` in `bytes / 4096` requests so far, `None`
/// for one without a reading of its disks.
fn sample(runner: &str, at: u64, bytes: Option<u64>) -> SampleRecord {
    SampleRecord {
        runner: runner.to_owned(),
        at,
        cpu: 0,
        cpu_usage: at,
        memory: 1000,
        network_received: 0,
        network_sent: 0,
        events: Events::default(),
        cpu_held: CpuHeld::default(),
        written: bytes.map(|bytes| Written {
            bytes,
            ios: bytes / 4096,
        }),
        dirty: bytes.map(|_| MIB),
    }
}

/// Job 1 on runner `r1`, whose container started at 1 000 000 ms and which took the job at 1 000 200 ms.
fn started(store: &Store) {
    store.queued("o/r", &queued(1), crate::time::now_ms());
    store.runner_started(runner("r1", 1_000_000));
    store.claimed(1, 10, "r1", 1_000_200);
}

async fn written(store: &Store) -> Option<api::DiskWritten> {
    store.flush().await;
    store.job(1).await.unwrap().unwrap().summary.metrics.disk
}

#[tokio::test]
async fn a_job_s_writes_leave_out_what_its_warm_runner_wrote_before_it_and_are_complete_once_its_end_was_read()
 {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    // Warm, it wrote 30M before the job; the job wrote 70M more.
    store.samples(vec![
        sample("r1", 1_000_050, Some(10 * MIB)),
        sample("r1", 1_000_150, Some(30 * MIB)),
        sample("r1", 1_000_300, Some(60 * MIB)),
        sample("r1", 1_000_400, Some(100 * MIB)),
    ]);
    let running = written(&store).await.unwrap();
    assert_eq!(running.bytes, 70 * MIB);
    assert_eq!(
        running.measure,
        Measure::Partial,
        "a running job's figure is a floor"
    );
    store.containers_ended(vec!["r1".to_owned()], 1_000_402);
    store.lost(1, 1_000_500);
    let done = written(&store).await.unwrap();
    assert_eq!(
        done,
        api::DiskWritten {
            bytes: 70 * MIB,
            ios: 70 * MIB / 4096,
            dirty: Some(MIB),
            measure: Measure::Complete,
        }
    );
}

#[tokio::test]
async fn a_job_closed_before_the_sampler_saw_its_cgroup_go_gets_its_whole_figure_once_it_does() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    store.samples(vec![
        sample("r1", 1_000_100, Some(0)),
        sample("r1", 1_000_300, Some(40 * MIB)),
    ]);
    store.lost(1, 1_000_310);
    assert_eq!(
        written(&store).await.unwrap().measure,
        Measure::Partial,
        "closed before the end was seen"
    );
    store.containers_ended(vec!["r1".to_owned()], 1_000_320);
    let done = written(&store).await.unwrap();
    assert_eq!(done.bytes, 40 * MIB);
    assert_eq!(
        done.measure,
        Measure::Complete,
        "the end seen after the close"
    );
}

#[tokio::test]
async fn a_restart_mid_job_loses_nothing_and_counts_nothing_twice() {
    let dir = TestDir::new("disk");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    started(&store);
    store.samples(vec![
        sample("r1", 1_000_100, Some(5 * MIB)),
        sample("r1", 1_000_300, Some(25 * MIB)),
    ]);
    store.flush().await;
    drop(store);
    // The counters went on while no daemon ran; the next one reads where they got to.
    let store = Store::open(&path).unwrap();
    store.samples(vec![
        sample("r1", 1_000_900, Some(80 * MIB)),
        sample("r1", 1_001_000, Some(85 * MIB)),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_001_001);
    store.lost(1, 1_001_100);
    let done = written(&store).await.unwrap();
    assert_eq!(done.bytes, 80 * MIB);
    assert_eq!(done.measure, Measure::Complete);
}

#[tokio::test]
async fn a_job_whose_end_no_daemon_saw_is_partial() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    store.samples(vec![
        sample("r1", 1_000_100, Some(0)),
        sample("r1", 1_000_300, Some(40 * MIB)),
    ]);
    store.lost(1, 1_000_900);
    // A restart's recovery found it gone: no sampler saw it go.
    store.runners_gone(Vec::new(), 1_000_950);
    let done = written(&store).await.unwrap();
    assert_eq!(done.bytes, 40 * MIB);
    assert_eq!(done.measure, Measure::Partial);
}

#[tokio::test]
async fn a_counter_that_starts_again_counts_both_runs_of_it_and_is_partial() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    // `docker restart` gave the container a new cgroup: its counters start again from 0.
    store.samples(vec![
        sample("r1", 1_000_100, Some(10 * MIB)),
        sample("r1", 1_000_300, Some(50 * MIB)),
        sample("r1", 1_000_500, Some(8 * MIB)),
        sample("r1", 1_000_700, Some(20 * MIB)),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_000_701);
    store.lost(1, 1_000_800);
    let done = written(&store).await.unwrap();
    assert_eq!(done.bytes, 40 * MIB + 20 * MIB);
    assert_eq!(
        done.measure,
        Measure::Partial,
        "what it wrote between the last read and the restart is lost"
    );
}

#[tokio::test]
async fn a_baseline_without_a_reading_of_the_disks_makes_the_figure_partial() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    // The sampler learnt the disks only after the job started.
    store.samples(vec![
        sample("r1", 1_000_100, None),
        sample("r1", 1_000_300, Some(30 * MIB)),
        sample("r1", 1_000_500, Some(50 * MIB)),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_000_501);
    store.lost(1, 1_000_600);
    let done = written(&store).await.unwrap();
    assert_eq!(done.bytes, 20 * MIB);
    assert_eq!(done.measure, Measure::Partial);
}

#[tokio::test]
async fn a_job_without_a_reading_of_the_disks_has_no_figure_not_zero() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    // cgroup v1, a kernel without the io controller, or sysfs unreadable: no sample read the disks.
    store.samples(vec![
        sample("r1", 1_000_100, None),
        sample("r1", 1_000_300, None),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_000_301);
    store.lost(1, 1_000_400);
    assert_eq!(written(&store).await, None);
}

#[tokio::test]
async fn a_job_cancelled_before_a_runner_took_it_has_no_figure() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.queued("o/r", &queued(1), crate::time::now_ms());
    store.unanswered(1, 1_000_000);
    assert_eq!(written(&store).await, None);
}

#[tokio::test]
async fn a_history_of_schema_23_is_brought_to_this_one_and_its_jobs_have_no_figure() {
    let dir = TestDir::new("disk");
    let path = dir.0.join("runnerd.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(SCHEMA_23).unwrap();
    db.execute_batch(
        "INSERT INTO runners (name, repository, profile, slot, container, started_at, removed_at) \
         VALUES ('r1', 'o/r', 'app', 1, 'c1', 1000000, 1000500);\n\
         INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, conclusion, runner, \
         queued_at, started_at, finished_at, github_completed) \
         VALUES (1, 'o/r', 10, 1, 'test', '[]', 'completed', 'success', 'r1', 999000, 1000200, 1000400, 1);\n\
         INSERT INTO samples (runner, at, cpu, cpu_usage, memory, network_received, network_sent) \
         VALUES ('r1', 1000300, 0, 0, 1000, 0, 0);",
    )
    .unwrap();
    db.pragma_update(None, "user_version", 23).unwrap();
    drop(db);
    let store = Store::open(&path).unwrap();
    assert_eq!(user_version(&path), VERSION);
    let job = store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.metrics.disk, None);
    // The new columns take writes.
    store.queued("o/r", &queued(2), crate::time::now_ms());
    store.runner_started(runner("r2", 2_000_000));
    store.claimed(2, 20, "r2", 2_000_100);
    store.samples(vec![
        sample("r2", 2_000_050, Some(0)),
        sample("r2", 2_000_200, Some(MIB)),
    ]);
    store.containers_ended(vec!["r2".to_owned()], 2_000_201);
    store.lost(2, 2_000_300);
    store.flush().await;
    let job = store.job(2).await.unwrap().unwrap();
    assert_eq!(job.summary.metrics.disk.unwrap().bytes, MIB);
}

#[tokio::test]
async fn insights_read_the_drive_s_windows_and_pruning_drops_them_with_job_records() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let window = |start: u64, drive: Option<u64>| Window {
        start,
        end: start + 600_000,
        device: "nvme0n1".to_owned(),
        drive,
        slice: Some(MIB),
        daemon: Some(MIB),
        commits: None,
    };
    store.disk_window(window(1_000_000, Some(4 * MIB)));
    store.disk_window(window(1_600_000, None));
    store.flush().await;
    let history = store.insights(0).await.unwrap();
    assert_eq!(
        history.disk,
        [window(1_000_000, Some(4 * MIB)), window(1_600_000, None)],
        "a figure a reading lacked stays absent, never 0"
    );
    let keep = History::default();
    let keep_ms = u64::try_from(keep.jobs.as_millis()).unwrap();
    store.prune(1_700_000 + keep_ms, &keep).await.unwrap();
    assert_eq!(
        store.insights(0).await.unwrap().disk,
        [window(1_600_000, None)]
    );
}

#[tokio::test]
async fn a_last_sample_long_before_the_cgroup_went_leaves_the_figure_partial() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    // Restarted mid-job, the container's network could no longer be read: no sample for a minute before its end.
    store.samples(vec![
        sample("r1", 1_000_100, Some(0)),
        sample("r1", 1_000_300, Some(40 * MIB)),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_060_000);
    store.lost(1, 1_060_100);
    let done = written(&store).await.unwrap();
    assert_eq!(done.bytes, 40 * MIB);
    assert_eq!(
        done.measure,
        Measure::Partial,
        "the minute before the end was not read"
    );
}

#[tokio::test]
async fn a_container_seen_to_go_and_sampled_again_is_partial() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    // `docker restart`: the cgroup went, and its new counters had passed the old ones by the next read.
    store.samples(vec![
        sample("r1", 1_000_100, Some(0)),
        sample("r1", 1_000_300, Some(10 * MIB)),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_000_302);
    store.samples(vec![
        sample("r1", 1_000_500, Some(30 * MIB)),
        sample("r1", 1_000_700, Some(50 * MIB)),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_000_702);
    store.lost(1, 1_000_800);
    assert_eq!(
        written(&store).await.unwrap().measure,
        Measure::Partial,
        "what it wrote across the restart was not all read"
    );
}

#[tokio::test]
async fn a_counter_that_fell_before_the_job_s_first_sample_counts_from_zero_and_is_partial() {
    let dir = TestDir::new("disk");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    started(&store);
    // The warm runner had written 100M; started again around the job's start, its counters read 5M next.
    store.samples(vec![
        sample("r1", 1_000_100, Some(100 * MIB)),
        sample("r1", 1_000_300, Some(5 * MIB)),
        sample("r1", 1_000_500, Some(25 * MIB)),
    ]);
    store.containers_ended(vec!["r1".to_owned()], 1_000_502);
    store.lost(1, 1_000_600);
    let done = written(&store).await.unwrap();
    assert_eq!(done.bytes, 25 * MIB);
    assert_eq!(done.measure, Measure::Partial);
}
