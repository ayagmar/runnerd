//! What the writer writes, and how often: counters written only when they rise, runner names leased ahead in one
//! synced write, and telemetry committed together with each write kept or rolled back on its own.

use std::time::Duration;

use rusqlite::Connection;

use super::{queued, runner, sample};
use crate::api::{Event, HostSample};
use crate::observe::metrics::{CpuHeld, Events};
use crate::store::write::NAME_LEASE_SECS;
use crate::store::{LogChain, LogPosition, SampleRecord, Store};
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

const PREFIX: &str = "runnerd-0000abcd-";

/// The bug it catches: every runner start switching the database to `synchronous = FULL` for a write of its own, or a
/// lease that lets a name past what the history keeps go unwritten.
#[tokio::test]
async fn names_inside_a_lease_need_no_write_and_the_first_past_it_takes_a_new_one() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.runner_named(PREFIX, 1_000).await, Some(1_000));
    let fault = refuse_names(&path);
    assert_eq!(stored_lease(&fault), Some(1_000 + NAME_LEASE_SECS));

    // The history refuses writes now: every name inside the lease still goes, none past it.
    for secs in [1_001, 1_100, 1_000 + NAME_LEASE_SECS - 1] {
        assert_eq!(store.runner_named(PREFIX, secs).await, Some(secs));
    }
    assert_eq!(
        store.runner_named(PREFIX, 1_000 + NAME_LEASE_SECS).await,
        None,
        "the lease's end itself needs a new lease"
    );
    fault.execute_batch("DROP TRIGGER no_name").unwrap();
    assert_eq!(
        store.runner_named(PREFIX, 1_000 + NAME_LEASE_SECS).await,
        Some(1_000 + NAME_LEASE_SECS)
    );
    assert_eq!(stored_lease(&fault), Some(1_000 + 2 * NAME_LEASE_SECS));
}

/// The bug it catches: a restart whose engine could not read the history, or a clock stepped back, naming a runner
/// with a second a lease from before covered or a runner's row carries, which GitHub may still hold.
#[tokio::test]
async fn the_history_grants_a_second_past_every_name_it_knows_whatever_the_engine_proposes() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.runner_named(PREFIX, 5_000).await, Some(5_000));
        assert_eq!(store.runner_named(PREFIX, 5_001).await, Some(5_001));
        store.flush().await;
    }
    let ceiling = 5_000 + NAME_LEASE_SECS;
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.last_runner_second(PREFIX.to_owned()).await.unwrap(),
        Some(ceiling),
        "a restart, of this build or 0.1.5, reads the whole lease as named"
    );
    // The engine proposes a second inside the old lease, as one whose read of the history failed, behind a clock
    // stepped back, would.
    let fault = refuse_names(&path);
    assert_eq!(
        store.runner_named(PREFIX, 4_000).await,
        None,
        "a new writer writes before its first name"
    );
    fault.execute_batch("DROP TRIGGER no_name").unwrap();
    assert_eq!(store.runner_named(PREFIX, 4_000).await, Some(ceiling + 1));
    assert_eq!(
        store.runner_named(PREFIX, 4_001).await,
        Some(ceiling + 2),
        "a clock behind the last grant gets the next second"
    );
    assert_eq!(stored_lease(&fault), Some(ceiling + 1 + NAME_LEASE_SECS));
    drop(store);

    // A runner whose row names a later second than the lease, as one a clock ahead named.
    let store = Store::open(&path).unwrap();
    store.runner_started(runner(&format!("{PREFIX}box-app-1-9000"), 1));
    store.flush().await;
    assert_eq!(store.runner_named(PREFIX, 4_000).await, Some(9_001));

    // 0.1.5 wrote the second it named, past the lease it read; this build grants past it.
    drop(store);
    fault
        .execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('runner_named', ?1)",
            [20_000],
        )
        .unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(store.runner_named(PREFIX, 1).await, Some(20_001));
    assert_eq!(stored_lease(&fault), Some(20_001 + NAME_LEASE_SECS));
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

/// Events received so far: each log line as (job, line), each job sample as (job, `u64::MAX`), each host sample as
/// (0, its time).
fn received(
    events: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<crate::api::EventEnvelope>>,
) -> Vec<(u64, u64)> {
    let mut seen = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        match &envelope.event {
            Event::LogLine { job, line, .. } => seen.push((*job, *line)),
            Event::JobSample { job, .. } => seen.push((*job, u64::MAX)),
            Event::HostSample { sample } => seen.push((0, sample.at)),
            _ => {}
        }
    }
    seen
}

/// The bug it catches: a write that makes SQLite roll the whole batch back, as a full disk does, taking the writes
/// before it with it and leaving those after it untried, or the batch's events going out for writes not kept.
#[tokio::test]
async fn a_write_that_rolls_the_whole_batch_back_costs_the_others_nothing() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    for (id, name) in [(1, "r1"), (2, "boom")] {
        store.queued("o/r", &queued(id), crate::time::now_ms());
        store.runner_started(runner(name, 1_000_100));
        store.claimed(id, 10, name, 1_000_200);
    }
    store.flush().await;
    let fault = Connection::open(&path).unwrap();
    // RAISE(ROLLBACK) ends the whole transaction, savepoints and all, as SQLITE_FULL does.
    fault
        .execute_batch(
            "CREATE TRIGGER full BEFORE INSERT ON samples WHEN NEW.runner = 'boom' \
             BEGIN SELECT RAISE(ROLLBACK, 'database or disk is full'); END;",
        )
        .unwrap();
    let mut events = store.subscribe();
    let position = |offset| LogPosition { page: 1, offset };
    store.log(1, lines(1, 0), position(10), u64::MAX).await;
    store.samples(vec![sample("boom", 1_000_300, 0, 0)]);
    store.log(1, lines(1, 3), position(20), u64::MAX).await;
    store.samples(vec![sample("r1", 1_000_300, 0, 0)]);
    store.flush().await;

    assert_eq!(store.job(1).await.unwrap().unwrap().summary.log.lines, 6);
    assert_eq!(store.log_position(1).await.unwrap(), position(20));
    assert_eq!(
        received(&mut events),
        [
            (1, 0),
            (1, 1),
            (1, 2),
            (1, 3),
            (1, 4),
            (1, 5),
            (1, u64::MAX)
        ],
        "every write kept is announced in order, none of the one that failed"
    );
}

/// The bug it catches: a host sample's live event waiting on the database, which another connection may hold, though
/// it stores nothing; or lost with a batch that failed.
#[tokio::test]
async fn host_samples_go_out_while_the_database_is_held_and_when_their_batch_fails() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1), crate::time::now_ms());
    store.flush().await;
    let mut events = store.subscribe();
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let host = |at| HostSample {
        at,
        cpu: 100,
        memory: 1 << 30,
    };
    store.host_sample(host(1));
    // Past the telemetry window, well inside the writer's 5 s wait for the lock.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        received(&mut events),
        [(0, 1)],
        "a batch of host samples alone takes no lock"
    );

    // With log lines, the batch waits for the lock and gives up; the host sample still goes out, after them.
    store
        .log(1, lines(1, 0), LogPosition { page: 1, offset: 5 }, u64::MAX)
        .await;
    store.host_sample(host(2));
    tokio::time::sleep(Duration::from_millis(7000)).await;
    holder.execute_batch("COMMIT").unwrap();
    store.flush().await;
    assert_eq!(received(&mut events), [(0, 2)]);
    assert_eq!(store.job(1).await.unwrap().unwrap().summary.log.lines, 0);
}

/// The bug it catches: a capture's later lines stored after part of its log failed to be, moving the job's stored
/// position past lines it does not hold, so a capture that starts again from it never stores them.
#[tokio::test]
async fn a_log_chain_stops_at_lines_not_stored_and_a_capture_from_the_stored_position_fills_the_gap()
 {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1), crate::time::now_ms());
    store.flush().await;
    let start = store.log_position(1).await.unwrap();
    let fault = Connection::open(&path).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_opener BEFORE INSERT ON log_lines WHEN NEW.text LIKE '%opener%' \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    let position = |offset| LogPosition { page: 1, offset };
    let mut chain = LogChain::starting_at(Some(start));
    store
        .log_chained(1, vec!["a".to_owned()], position(10), u64::MAX, &mut chain)
        .await;
    store
        .log_chained(
            1,
            vec!["opener".to_owned()],
            position(20),
            u64::MAX,
            &mut chain,
        )
        .await;
    store
        .log_chained(
            1,
            vec!["closer".to_owned()],
            position(30),
            u64::MAX,
            &mut chain,
        )
        .await;
    store.flush().await;
    assert!(chain.refused(), "the capture is told to start again");
    assert_eq!(
        store.log_position(1).await.unwrap(),
        position(10),
        "the stored position stays before the lines not stored"
    );
    assert_eq!(
        store.log_lines(1, 0, 10).await.unwrap(),
        [(0, "a".to_owned())]
    );

    // A capture that starts from where its last one sent up to, past the gap, is refused too.
    let mut stale = LogChain::starting_at(Some(position(30)));
    store
        .log_chained(
            1,
            vec!["late".to_owned()],
            position(40),
            u64::MAX,
            &mut stale,
        )
        .await;
    store.flush().await;
    assert!(
        stale.refused(),
        "lines that do not start where the stored log ends are refused"
    );

    fault.execute_batch("DROP TRIGGER no_opener").unwrap();
    let mut again = LogChain::starting_at(Some(store.log_position(1).await.unwrap()));
    for (text, offset) in [("opener", 20), ("closer", 30)] {
        store
            .log_chained(
                1,
                vec![text.to_owned()],
                position(offset),
                u64::MAX,
                &mut again,
            )
            .await;
    }
    store.flush().await;
    assert!(
        !again.refused(),
        "the capture from the stored position is kept"
    );
    assert_eq!(
        store.log_lines(1, 0, 10).await.unwrap(),
        [
            (0, "a".to_owned()),
            (1, "opener".to_owned()),
            (2, "closer".to_owned())
        ]
    );
    assert_eq!(store.log_position(1).await.unwrap(), position(30));
}
