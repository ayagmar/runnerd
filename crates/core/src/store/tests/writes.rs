//! What the writer writes, and how often: counters written only when they rise, runner names leased ahead in one
//! synced write, and telemetry committed together with each write kept or rolled back on its own.

use std::time::Duration;

use rusqlite::Connection;

use super::{queued, runner, sample};
use crate::api::Event;
use crate::observe::metrics::{CpuHeld, Events};
use crate::store::write::NAME_LEASE_SECS;
use crate::store::{LogPosition, SampleRecord, Store};
use crate::testdir::TestDir;

/// A trigger on `path` that counts each update of a runner's memory and socket counters in `counted`: an update that
/// matches no row fires none.
fn count_counter_writes(path: &std::path::Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE counted (what TEXT NOT NULL);
         CREATE TRIGGER count_ooms AFTER UPDATE OF ooms ON runners BEGIN INSERT INTO counted VALUES ('ooms'); END;
         CREATE TRIGGER count_local AFTER UPDATE OF local_ooms ON runners
         BEGIN INSERT INTO counted VALUES ('local_ooms'); END;
         CREATE TRIGGER count_sock AFTER UPDATE OF sock_throttled ON runners
         BEGIN INSERT INTO counted VALUES ('sock_throttled'); END;",
    )
    .unwrap();
    db
}

fn counted(db: &Connection) -> u64 {
    db.query_row("SELECT count(*) FROM counted", [], |row| row.get(0))
        .unwrap()
}

fn counters(oom: Option<u64>, local_oom: Option<u64>, sock: Option<u64>) -> SampleRecord {
    SampleRecord {
        cpu_held: CpuHeld::default(),
        events: Events {
            oom,
            oom_kill: None,
            sock_throttled: sock,
            local_oom,
        },
        ..sample("r1", 1_000_300, 0, 0)
    }
}

/// The bug it catches: every 2 s sample rewriting a runner's row for counters that did not move (three B-tree writes
/// per runner per sample), or, filtering zeros out to save them, a kernel that counts and saw none reading as one
/// without the counter.
#[tokio::test]
async fn a_runners_counters_are_written_when_first_read_and_when_they_rise_only() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1), crate::time::now_ms());
    store.runner_started(runner("r1", 1_000_100));
    store.claimed(1, 10, "r1", 1_000_200);
    store.flush().await;
    let fault = count_counter_writes(&path);

    store.samples(vec![counters(None, None, None)]);
    store.flush().await;
    assert_eq!(
        counted(&fault),
        0,
        "a kernel without the counters writes none"
    );

    store.samples(vec![counters(Some(0), Some(0), Some(0))]);
    store.flush().await;
    assert_eq!(
        counted(&fault),
        3,
        "unknown becomes 0 once for each counter"
    );
    let job = store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.metrics.sock_throttled, Some(0));

    for _ in 0..5 {
        store.samples(vec![counters(Some(0), Some(0), Some(0))]);
    }
    store.flush().await;
    assert_eq!(counted(&fault), 3, "the same counts again write nothing");

    store.samples(vec![counters(Some(2), Some(0), Some(0))]);
    store.samples(vec![counters(Some(1), Some(0), Some(0))]);
    store.flush().await;
    assert_eq!(
        counted(&fault),
        4,
        "only the rise writes; a lower count does not"
    );

    store.lost(1, 1_000_400);
    store.flush().await;
    let jobs = store.sizing(0).await.unwrap().jobs;
    assert_eq!(
        jobs.iter()
            .map(|job| (job.hit_cap, job.own_cap))
            .collect::<Vec<_>>(),
        [(Some(true), Some(false))],
        "a counted 0 reads as the cap held, not as unknown"
    );
}

/// A trigger on `path` that refuses every write of `runner_named`, as a full disk would; dropping the connection does
/// not drop it.
fn refuse_names(path: &std::path::Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TRIGGER no_name BEFORE INSERT ON meta WHEN NEW.key = 'runner_named' \
         BEGIN SELECT RAISE(ABORT, 'full'); END;",
    )
    .unwrap();
    db
}

fn stored_lease(db: &Connection) -> Option<u64> {
    db.query_row(
        "SELECT value FROM meta WHERE key = 'runner_named'",
        [],
        |row| row.get(0),
    )
    .ok()
}

/// The bug it catches: every runner start switching the database to `synchronous = FULL` for a write of its own, or a
/// lease that lets a name past what the history keeps go unwritten.
#[tokio::test]
async fn names_inside_a_lease_need_no_write_and_the_first_past_it_takes_a_new_one() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    assert!(
        store.runner_named(1_000).await,
        "the first name takes a lease"
    );
    let fault = refuse_names(&path);
    assert_eq!(stored_lease(&fault), Some(1_000 + NAME_LEASE_SECS));

    // The history refuses writes now: every name inside the lease still goes, none past it.
    for secs in [1_001, 1_100, 1_000 + NAME_LEASE_SECS - 1] {
        assert!(store.runner_named(secs).await, "{secs} is inside the lease");
    }
    assert!(
        !store.runner_named(1_000 + NAME_LEASE_SECS).await,
        "the lease's end itself needs a new lease"
    );
    fault.execute_batch("DROP TRIGGER no_name").unwrap();
    assert!(store.runner_named(1_000 + NAME_LEASE_SECS).await);
    assert_eq!(stored_lease(&fault), Some(1_000 + 2 * NAME_LEASE_SECS));
}

/// The bug it catches: a restart, or a clock stepped back, naming a runner with a second a lease from before already
/// covered, which a runner may hold, as the history knew it only by the lease.
#[tokio::test]
async fn a_restart_names_past_the_lease_and_a_clock_stepped_back_never_lowers_it() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let prefix = "runnerd-0000abcd-".to_owned();
    {
        let store = Store::open(&path).unwrap();
        assert!(store.runner_named(5_000).await);
        assert!(store.runner_named(5_001).await);
        store.flush().await;
    }
    let store = Store::open(&path).unwrap();
    let ceiling = 5_000 + NAME_LEASE_SECS;
    assert_eq!(
        store.last_runner_second(prefix.clone()).await.unwrap(),
        Some(ceiling),
        "a restart, this build or 0.1.5, names past the whole lease"
    );
    // A writer after a restart takes a lease of its own on its first name, even one the clock put before the stored
    // one; what is stored never goes back.
    let fault = refuse_names(&path);
    assert!(
        !store.runner_named(4_000).await,
        "a new writer writes before its first name"
    );
    fault.execute_batch("DROP TRIGGER no_name").unwrap();
    assert!(store.runner_named(4_000).await);
    assert_eq!(
        store.last_runner_second(prefix).await.unwrap(),
        Some(ceiling)
    );

    // 0.1.5 wrote the second it named, past the lease it read; this build reads it as the latest and leases past it.
    fault
        .execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('runner_named', ?1)",
            [ceiling + 10],
        )
        .unwrap();
    let store = Store::open(&path).unwrap();
    assert!(store.runner_named(ceiling + 11).await);
    assert_eq!(stored_lease(&fault), Some(ceiling + 11 + NAME_LEASE_SECS));
}

fn lines(job: u64, from: u64) -> Vec<String> {
    (from..from + 3)
        .map(|n| format!("job {job} line {n}"))
        .collect()
}

/// The bug it catches: one telemetry write that fails rolling back the others committed with it, or its events
/// going out though its lines were not kept.
#[tokio::test]
async fn telemetry_committed_together_keeps_each_write_or_rolls_it_back_alone() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    for id in [1, 2] {
        store.queued("o/r", &queued(id), crate::time::now_ms());
    }
    store.runner_started(runner("r1", 1_000_100));
    store.claimed(1, 10, "r1", 1_000_200);
    store.flush().await;
    let fault = Connection::open(&path).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_job_2 BEFORE INSERT ON log_lines WHEN NEW.job = 2 \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    let before = store.log_position(2).await.unwrap();
    let mut events = store.subscribe();
    let position = |offset| LogPosition { page: 1, offset };
    // Sent together, so the writer gathers them into one transaction.
    store.log(1, lines(1, 0), position(10), u64::MAX).await;
    store.samples(vec![sample("r1", 1_000_300, 0, 0)]);
    store.log(2, lines(2, 0), position(20), u64::MAX).await;
    store.log(1, lines(1, 3), position(30), u64::MAX).await;
    store.flush().await;

    let job_1 = store.job(1).await.unwrap().unwrap().summary;
    assert_eq!(
        job_1.log.lines, 6,
        "job 1's lines on both sides of the failure"
    );
    assert_eq!(store.log_position(1).await.unwrap(), position(30));
    assert_eq!(store.job(2).await.unwrap().unwrap().summary.log.lines, 0);
    assert_eq!(
        store.log_position(2).await.unwrap(),
        before,
        "job 2's capture resumes where it was"
    );
    let mut announced = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        match &envelope.event {
            Event::LogLine { job, line, .. } => announced.push((*job, *line)),
            Event::JobSample { job, .. } => announced.push((*job, u64::MAX)),
            _ => {}
        }
    }
    assert_eq!(
        announced,
        [
            (1, 0),
            (1, 1),
            (1, 2),
            (1, u64::MAX),
            (1, 3),
            (1, 4),
            (1, 5)
        ],
        "in the order sent, none of job 2's"
    );
}

/// The bug it catches: telemetry's events going out before its transaction commits, so a client that reads what an
/// event names finds nothing yet, or reads it gone after a crash.
#[tokio::test]
async fn telemetry_is_announced_only_once_it_commits() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1), crate::time::now_ms());
    store.runner_started(runner("r1", 1_000_100));
    store.claimed(1, 10, "r1", 1_000_200);
    store.flush().await;
    let mut events = store.subscribe();
    // Another process holds the database: the writer waits to begin.
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    store
        .log(1, lines(1, 0), LogPosition { page: 1, offset: 5 }, u64::MAX)
        .await;
    // Longer than the telemetry window: the lines wait for the lock, not for company.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        events.try_recv().is_err(),
        "nothing is announced while the lines are not committed"
    );
    holder.execute_batch("COMMIT").unwrap();
    store.flush().await;
    let first = events.try_recv().unwrap();
    assert!(matches!(first.event, Event::LogLine { line: 0, .. }));
    assert_eq!(store.job(1).await.unwrap().unwrap().summary.log.lines, 3);
}
