//! The history database: migrations, the log cap, and job listing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use super::{
    LOG_BACKLOG, LogPosition, MIGRATIONS, MirrorPosition, RunnerRecord, SampleRecord, Store,
    StoreError,
};
use crate::api::{JobQuery, JobState, StatsQuery};
use crate::github::QueuedJob;
use crate::testdir::TestDir;

fn queued(id: u64) -> QueuedJob {
    QueuedJob {
        id,
        run_id: 10,
        name: format!("job {id}"),
        labels: vec!["self-hosted".to_owned()],
        created_at: 1_000_000 + id,
        run_attempt: 1,
        ..QueuedJob::default()
    }
}

#[tokio::test]
async fn a_fresh_database_migrates_to_the_latest_version() {
    let dir = TestDir::new("store");
    let path = dir.0.join("nested/runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1));
    store.flush().await;
    let job = store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Queued);
    let version: usize = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, MIGRATIONS.len());
    // Opening it again runs nothing twice.
    drop(store);
    assert!(Store::open(&path).is_ok());
}

#[tokio::test]
async fn a_job_recorded_before_runners_kept_their_caps_reads_without_them() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(MIGRATIONS[0]).unwrap();
        db.execute_batch(
            "INSERT INTO runners (name, repository, profile, slot, container, started_at) \
             VALUES ('r1', 'o/r', 'app', 1, 'c1', 1000); \
             INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, runner, queued_at) \
             VALUES (1, 'o/r', 10, 1, 'build', '[]', 'running', 'r1', 900);",
        )
        .unwrap();
        db.pragma_update(None, "user_version", 1).unwrap();
    }
    let store = Store::open(&path).unwrap();
    let runner = store.job(1).await.unwrap().unwrap().summary.runner.unwrap();
    assert_eq!((runner.name.as_str(), runner.slot), ("r1", 1));
    assert_eq!((runner.memory_cap, runner.cpu_cap), (None, None));
}

#[test]
fn a_database_from_a_newer_runnerd_is_refused() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let error = Store::open(&path).unwrap_err();
    assert!(
        matches!(error, StoreError::Newer { found: 99, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("newer"), "{error}");
}

#[tokio::test]
async fn a_log_stops_at_its_cap_and_says_so() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.queued("o/r", &queued(1));
    let position = LogPosition { page: 1, offset: 0 };
    // Each line takes its length plus a newline: 6 bytes.
    store
        .log(1, vec!["line1".to_owned(); 3], position, 15)
        .await;
    store.log(1, vec!["line2".to_owned()], position, 15).await;
    store.flush().await;
    let job = store.job(1).await.unwrap().unwrap();
    assert_eq!((job.summary.log.lines, job.summary.log.bytes), (2, 12));
    assert!(job.summary.log.truncated);
    assert_eq!(store.log_lines(1, 0, 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn jobs_list_newest_first_by_repository_in_pages() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    for id in 1..=5 {
        store.queued(if id % 2 == 0 { "o/even" } else { "o/odd" }, &queued(id));
    }
    store.flush().await;
    let page = |before| {
        store.jobs(JobQuery {
            repository: Some("o/odd".to_owned()),
            before,
            limit: Some(2),
            ..JobQuery::default()
        })
    };
    let first = page(None).await.unwrap();
    let ids: Vec<u64> = first.jobs.iter().map(|job| job.id).collect();
    assert_eq!((ids, first.next), (vec![5, 3], Some(3)));
    let second = page(first.next).await.unwrap();
    let ids: Vec<u64> = second.jobs.iter().map(|job| job.id).collect();
    assert_eq!((ids, second.next), (vec![1], None));
}

fn runner(name: &str, started_at: u64) -> RunnerRecord {
    RunnerRecord {
        name: name.to_owned(),
        repository: "o/r".to_owned(),
        profile: "app".to_owned(),
        slot: 1,
        container: format!("{name}-container"),
        started_at,
        memory_cap: Some(4 << 30),
        cpu_cap: Some(2000),
    }
}

#[tokio::test]
async fn stats_count_a_lost_job_without_a_conclusion_as_failed() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.queued("o/r", &queued(1));
    store.runner_started(runner("r1", 1_000_100));
    store.claimed(1, 10, "r1", 1_000_200);
    store.lost(1, 1_000_300);
    store.flush().await;
    let stats = store
        .stats(
            StatsQuery {
                since: Some(0),
                ..StatsQuery::default()
            },
            2_000,
        )
        .await
        .unwrap();
    let counts: Vec<(u64, u64, u64, u64)> = stats
        .rows
        .iter()
        .map(|row| (row.jobs, row.succeeded, row.failed, row.cancelled))
        .collect();
    assert_eq!(counts, [(1, 0, 1, 0)]);
}

fn sample(runner: &str, at: u64, cpu_usage: u64, received: u64) -> SampleRecord {
    SampleRecord {
        runner: runner.to_owned(),
        at,
        cpu: 0,
        cpu_usage,
        memory: 1000,
        network_received: received,
        network_sent: received / 10,
    }
}

/// Runs job 1 on runner `r1` from 1 000 200 to 1 000 400 ms with `samples`, and returns its metrics.
async fn metrics_of(samples: Vec<SampleRecord>) -> crate::api::JobMetrics {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.queued("o/r", &queued(1));
    store.runner_started(runner("r1", 1_000_100));
    store.claimed(1, 10, "r1", 1_000_200);
    store.samples(samples);
    store.lost(1, 1_000_400);
    store.flush().await;
    store.job(1).await.unwrap().unwrap().summary.metrics
}

#[tokio::test]
async fn a_job_with_one_sample_counts_its_container_usage_from_zero() {
    let metrics = metrics_of(vec![sample("r1", 1_000_300, 2_500_000, 10_000_000)]).await;
    assert_eq!(metrics.cpu_time, Some(2500));
    assert_eq!(metrics.network_received, Some(10_000_000));
    assert_eq!(metrics.network_sent, Some(1_000_000));
}

#[tokio::test]
async fn a_job_counts_its_usage_from_the_last_sample_before_it_started() {
    let metrics = metrics_of(vec![
        sample("r1", 1_000_150, 1_000_000, 1_000),
        sample("r1", 1_000_250, 2_000_000, 5_000_000),
        sample("r1", 1_000_350, 3_500_000, 11_001_000),
    ])
    .await;
    assert_eq!(metrics.cpu_time, Some(2500));
    assert_eq!(metrics.network_received, Some(11_000_000));
}

#[tokio::test]
async fn a_change_whose_event_cannot_be_stored_is_not_made_or_announced() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    let mut events = store.subscribe();
    let fault = Connection::open(&path).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_events BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    store.queued("o/r", &queued(1));
    store.flush().await;
    assert_eq!(store.job(1).await.unwrap(), None);
    assert!(events.try_recv().is_err());

    // Once events can be stored again, the same change goes through with its event.
    fault.execute_batch("DROP TRIGGER no_events").unwrap();
    store.queued("o/r", &queued(1));
    store.flush().await;
    assert!(store.job(1).await.unwrap().is_some());
    let stored = store.events_after(0, 10).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(events.try_recv().unwrap().id, stored[0].id);
}

#[tokio::test]
async fn mirror_bytes_and_the_log_position_are_counted_together_or_not_at_all() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.runner_started(runner("a", 1));
    store.runner_started(runner("b", 1));
    store.flush().await;
    let until = MirrorPosition { at: 500, seen: 2 };
    let fault = Connection::open(&path).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_b BEFORE UPDATE OF mirror_bytes ON runners WHEN new.name = 'b' \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    assert!(
        !store
            .mirror_bytes(vec![("a".to_owned(), 10), ("b".to_owned(), 20)], until)
            .await
    );
    store.flush().await;
    let counted = |db: &Connection| -> Vec<u64> {
        db.prepare("SELECT mirror_bytes FROM runners ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(counted(&fault), [0, 0]);
    assert_eq!(
        store.mirror_log_until().await.unwrap(),
        MirrorPosition::default()
    );

    // Read again from the same position, the batch counts once.
    fault.execute_batch("DROP TRIGGER no_b").unwrap();
    assert!(
        store
            .mirror_bytes(vec![("a".to_owned(), 10), ("b".to_owned(), 20)], until)
            .await
    );
    store.flush().await;
    assert_eq!(counted(&fault), [10, 20]);
    assert_eq!(store.mirror_log_until().await.unwrap(), until);
}

#[tokio::test]
async fn log_lines_wait_while_the_writer_is_behind_by_the_backlog() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1));
    store.flush().await;
    // Another process holds the database: the writer waits.
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let sent = Arc::new(AtomicU64::new(0));
    let producer = tokio::spawn({
        let (store, sent) = (store.clone(), sent.clone());
        async move {
            // A noisy job: 64 MiB of log, in 1 MiB batches.
            for _ in 0..64 {
                let line = "x".repeat((1 << 20) - 1);
                store
                    .log(1, vec![line], LogPosition { page: 1, offset: 0 }, u64::MAX)
                    .await;
                sent.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let queued_mib = sent.load(Ordering::SeqCst);
    holder.execute_batch("COMMIT").unwrap();
    producer.await.unwrap();
    assert!(
        queued_mib <= LOG_BACKLOG / (1 << 20) + 1,
        "{queued_mib} MiB queued"
    );
    store.flush().await;
    assert_eq!(store.job(1).await.unwrap().unwrap().summary.log.lines, 64);
}

#[tokio::test]
async fn a_log_batch_larger_than_the_backlog_waits_for_the_writer_too() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1));
    store.flush().await;
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    // One batch of twice the backlog.
    let mib = usize::try_from((2 * LOG_BACKLOG) >> 20).unwrap();
    let batch = vec!["z".repeat((1 << 20) - 1); mib];
    let producer = tokio::spawn({
        let store = store.clone();
        async move {
            store
                .log(1, batch, LogPosition { page: 1, offset: 9 }, u64::MAX)
                .await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let waited = !producer.is_finished();
    holder.execute_batch("COMMIT").unwrap();
    producer.await.unwrap();
    assert!(waited, "the whole batch was queued past the backlog");
    store.flush().await;
    assert_eq!(
        store.job(1).await.unwrap().unwrap().summary.log.lines,
        mib as u64
    );
    assert_eq!(
        store.log_position(1).await.unwrap(),
        LogPosition { page: 1, offset: 9 }
    );
}
