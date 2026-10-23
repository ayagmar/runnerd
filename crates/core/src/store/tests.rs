//! The history database: migrations, the log cap, and job listing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::Connection;

use super::{
    LOG_BACKLOG, LogPosition, MIGRATIONS, Measured, MirrorPosition, RunnerRecord, SampleRecord,
    Store, StoreError,
};
use crate::api::{Bucket, HostSample, JobQuery, JobState, StatsQuery};
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

#[tokio::test]
async fn cache_sizes_now_are_the_last_measurement_of_each_volume() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    assert_eq!(store.cache_sizes_now().await.unwrap(), Measured::default());
    let sizes = |pnpm: u64, maven: u64| {
        BTreeMap::from([
            ("runnerd-pnpm".to_owned(), pnpm),
            ("runnerd-maven".to_owned(), maven),
        ])
    };
    store.cache_sizes(1_000, sizes(10, 20));
    store.cache_sizes(61_000, sizes(30, 40));
    store.flush().await;
    let measured = store.cache_sizes_now().await.unwrap();
    assert_eq!(measured.at, Some(61));
    assert_eq!(
        measured.bytes,
        BTreeMap::from([
            ("runnerd-maven".to_owned(), 40),
            ("runnerd-pnpm".to_owned(), 30),
        ])
    );
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

#[tokio::test]
async fn hourly_stats_count_at_most_the_last_31_days() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.queued("o/r", &queued(1));
    store.runner_started(runner("r1", 1_000_100));
    store.claimed(1, 10, "r1", 1_000_200);
    store.lost(1, 1_000_300);
    store.flush().await;
    // Forty days after the job.
    let now = 1_000 + 40 * 24 * 3600;
    let jobs = |bucket| {
        let store = store.clone();
        async move {
            let query = StatsQuery {
                since: Some(0),
                bucket,
                ..StatsQuery::default()
            };
            let stats = store.stats(query, now).await.unwrap();
            stats.rows.iter().map(|row| row.jobs).sum::<u64>()
        }
    };
    assert_eq!(jobs(Bucket::Hour).await, 0);
    assert_eq!(jobs(Bucket::Day).await, 1);
}

#[tokio::test]
async fn a_host_sample_reads_back_at_once_and_is_kept_from_a_flush_on() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    let sample = HostSample {
        at: 100,
        cpu: 1500,
        memory: 1 << 30,
    };
    store.host_sample(sample.clone());
    assert_eq!(
        store.host_samples(0).await.unwrap(),
        std::slice::from_ref(&sample)
    );
    store.flush().await;
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.host_samples(0).await.unwrap(), [sample]);
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
    tokio::time::sleep(Duration::from_millis(300)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;
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

fn repository(name: &str) -> crate::api::RepositoryStatus {
    crate::api::RepositoryStatus {
        name: name.to_owned(),
        profile: "app".to_owned(),
        labels: vec!["self-hosted".to_owned()],
        paused: false,
        queued_jobs: 0,
    }
}

fn profile(name: &str) -> crate::api::ProfileStatus {
    crate::api::ProfileStatus {
        name: name.to_owned(),
        image: format!("runnerd-runner:{name}"),
        cpus: 2,
        memory: 4 << 30,
        reseeding: false,
        slots: Vec::new(),
    }
}

#[tokio::test]
async fn pruning_keeps_what_a_restart_needs_to_announce_a_removed_repository_or_profile() {
    use crate::api::Event;
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.event(Event::RepositoryChanged {
        repository: repository("o/r"),
    });
    store.event(Event::RepositoryChanged {
        repository: repository("o/s"),
    });
    store.event(Event::RepositoryRemoved {
        repository: "o/s".to_owned(),
    });
    store.event(Event::ProfileChanged {
        profile: profile("app"),
    });
    store.event(Event::ProfileChanged {
        profile: profile("small"),
    });
    store.event(Event::ProfileRemoved {
        profile: "small".to_owned(),
    });
    store.event(Event::BudgetChanged {
        budget: crate::api::Budget {
            slice: "ci.slice".to_owned(),
            memory_max: None,
            memory_in_use: 0,
            mirror_memory: 0,
            seed_memory: Vec::new(),
            shell_memory: Vec::new(),
        },
    });
    store.flush().await;

    // Long after: every event is older than the history keeps.
    let later = crate::time::now_ms() + 400 * 24 * 3600 * 1000;
    store
        .prune(later, &crate::config::History::default())
        .await
        .unwrap();

    let announced = store.announced().unwrap();
    assert_eq!(announced.repositories, [repository("o/r")]);
    assert_eq!(announced.profiles, [profile("app")]);
    let kept: Vec<_> = store
        .events_after(0, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|envelope| envelope.event)
        .collect();
    assert!(
        !kept
            .iter()
            .any(|event| matches!(event, Event::BudgetChanged { .. })),
        "other events are still pruned: {kept:?}"
    );
}

const DAY_MS: u64 = 24 * 3600 * 1000;

#[tokio::test]
async fn pruning_drops_samples_and_logs_then_finished_jobs_past_their_ages_and_keeps_open_jobs() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.queued("o/r", &queued(1));
    store.runner_started(runner("r1", 1_000_100));
    store.claimed(1, 10, "r1", 1_000_200);
    store.samples(vec![sample("r1", 1_000_250, 1, 1)]);
    store
        .log(
            1,
            vec!["built".to_owned()],
            LogPosition { page: 1, offset: 6 },
            u64::MAX,
        )
        .await;
    store.runner_removed("r1", 1_000_290);
    store.lost(1, 1_000_300);
    store.queued("o/r", &queued(2));
    store.flush().await;
    let history = crate::config::History::default();
    let samples = |store: Store| async move {
        let series = store.job_samples(1, Duration::from_secs(2)).await.unwrap();
        series.map(|series| series.samples.len())
    };
    assert_eq!(samples(store.clone()).await, Some(1));

    // Past how long samples and logs are kept, not job records.
    store
        .prune(1_000_300 + 31 * DAY_MS, &history)
        .await
        .unwrap();
    assert_eq!(samples(store.clone()).await, Some(0));
    assert_eq!(store.log_lines(1, 0, 10).await.unwrap(), []);
    assert!(
        store.job(1).await.unwrap().is_some(),
        "job 1's record stays"
    );

    store
        .prune(1_000_300 + 91 * DAY_MS, &history)
        .await
        .unwrap();
    assert_eq!(store.job(1).await.unwrap(), None);
    assert_eq!(
        store.job(2).await.unwrap().map(|job| job.summary.state),
        Some(JobState::Queued)
    );
}

#[tokio::test]
async fn a_closed_store_stops_a_prune_after_its_batch() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    for at in 1..=10_000 {
        store.host_sample(HostSample {
            at,
            cpu: 1,
            memory: 1,
        });
    }
    store.flush().await;
    // Holds the write lock, so the prune's first batch waits for it.
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let prune = store.prune(400 * DAY_MS, &crate::config::History::default());
    tokio::time::sleep(Duration::from_millis(200)).await;
    store.close();
    holder.execute_batch("COMMIT").unwrap();
    prune.await.unwrap();
    assert_eq!(store.host_samples(0).await.unwrap().len(), 8_000);
}

#[tokio::test]
async fn a_history_from_before_version_4_keeps_the_latest_event_of_each_repository_through_pruning()
{
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    {
        let db = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..3] {
            db.execute_batch(migration).unwrap();
        }
        db.pragma_update(None, "user_version", 3).unwrap();
        let event = |event: crate::api::Event| {
            db.execute(
                "INSERT INTO events (at, body) VALUES (1000, ?1)",
                [serde_json::to_string(&event).unwrap()],
            )
            .unwrap();
        };
        event(crate::api::Event::RepositoryChanged {
            repository: repository("o/r"),
        });
        event(crate::api::Event::RepositoryChanged {
            repository: repository("o/s"),
        });
        event(crate::api::Event::RepositoryRemoved {
            repository: "o/s".to_owned(),
        });
    }
    let store = Store::open(&path).unwrap();
    store
        .prune(400 * DAY_MS, &crate::config::History::default())
        .await
        .unwrap();
    assert_eq!(store.announced().unwrap().repositories, [repository("o/r")]);
    assert_eq!(store.events_after(0, 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_budget_event_an_older_daemon_stored_still_reads() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    // As runnerd 0.0.5 stored it, without the mirror's and seed containers' memory.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            r#"INSERT INTO events (at, body) VALUES (1000,
                '{"type":"budget.changed","budget":{"slice":"ci.slice","memory_max":null,"memory_in_use":0}}')"#,
        )
        .unwrap();
    assert_eq!(store.events_after(0, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_repository_event_that_does_not_read_fails_the_startup_read() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.event(crate::api::Event::RepositoryChanged {
        repository: repository("o/r"),
    });
    store.flush().await;
    Connection::open(&path)
        .unwrap()
        .execute_batch(r#"UPDATE events SET body = '{"type":"repository.changed","repository":7}'"#)
        .unwrap();
    assert!(
        store.announced().is_err(),
        "an incomplete list would keep a removed repository in resuming clients"
    );
}

#[tokio::test]
async fn sizing_reads_finished_jobs_peaks_and_leaves_cancelled_ones_out() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    for (id, name) in [(1, "r1"), (2, "r2")] {
        store.queued("o/r", &queued(id));
        store.runner_started(runner(name, 1_000_100));
        store.claimed(id, 10, name, 1_000_200);
    }
    let memory = |runner: &str, at, memory| SampleRecord {
        memory,
        ..sample(runner, at, 0, 0)
    };
    store.samples(vec![
        memory("r1", 1_000_250, 1 << 30),
        memory("r1", 1_000_300, 3 << 30),
        memory("r2", 1_000_300, 2 << 30),
    ]);
    store.lost(1, 1_000_400);
    store.details(crate::github::JobDetails {
        id: 2,
        run_id: 10,
        run_attempt: 1,
        name: "job 2".to_owned(),
        workflow_name: None,
        head_branch: None,
        html_url: None,
        labels: Vec::new(),
        created_at: None,
        status: "completed".to_owned(),
        conclusion: Some("cancelled".to_owned()),
        started_at: None,
        completed_at: None,
        runner_name: Some("r2".to_owned()),
        steps: Vec::new(),
    });
    store.runner_removed("r2", 1_000_500);
    store.flush().await;
    let history = store.sizing(0).await.unwrap();
    let peaks: Vec<_> = history
        .jobs
        .iter()
        .map(|job| (job.name.as_str(), job.memory_peak, job.memory_cap))
        .collect();
    assert_eq!(peaks, [("job 1", 3 << 30, Some(4 << 30))]);
    assert_eq!(history.runners.len(), 2, "both runners held a slot");
    // `runnerd doctor` reads the same without a daemon.
    assert_eq!(super::sizing_at(&path, 0).unwrap(), Some(history));
    assert_eq!(super::sizing_at(&dir.0.join("none.db"), 0).unwrap(), None);
}

/// A database at `path` written by a runnerd that knew the first `version` migrations, in WAL mode as the daemon
/// leaves it, with one finished job that peaked at 3G in a runner of `node`.
fn database_of_version(path: &std::path::Path, version: usize) {
    let db = Connection::open(path).unwrap();
    db.pragma_update(None, "journal_mode", "wal").unwrap();
    for migration in &MIGRATIONS[..version] {
        db.execute_batch(migration).unwrap();
    }
    db.pragma_update(None, "user_version", version).unwrap();
    db.execute_batch(
        "INSERT INTO runners (name, repository, profile, slot, container, started_at, removed_at) \
         VALUES ('r1', 'o/r', 'node', 1, 'c1', 1000, 9000); \
         INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, conclusion, runner, \
         queued_at, started_at, finished_at, memory_peak, cpu_peak) \
         VALUES (1, 'o/r', 10, 1, 'build', '[]', 'completed', 'success', 'r1', 1000, 2000, 8000, 3221225472, 900);",
    )
    .unwrap();
}

#[test]
fn doctor_reads_the_history_in_a_directory_it_cannot_write() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    database_of_version(&path, MIGRATIONS.len());
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o555)).unwrap();
    let history = super::sizing_at(&path, 0);
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let peaks: Vec<_> = history
        .unwrap()
        .unwrap()
        .jobs
        .iter()
        .map(|job| job.memory_peak)
        .collect();
    assert_eq!(peaks, [3 << 30]);
}

#[test]
fn doctor_reads_the_peaks_of_a_history_from_before_runners_had_caps() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    database_of_version(&path, 1);
    let history = super::sizing_at(&path, 0).unwrap().unwrap();
    let peaks: Vec<_> = history
        .jobs
        .iter()
        .map(|job| (job.memory_peak, job.memory_cap))
        .collect();
    assert_eq!(peaks, [(3 << 30, None)]);
    let version: u32 = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1, "doctor does not migrate it");
}
