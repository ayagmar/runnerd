//! Pruning: deletes what is older than `[history]` keeps, on its own connection off the writer thread, a batch per
//! transaction with a pause after each as long as the batch took, so the writer's transactions get the lock in
//! between. It keeps each repository's latest `repository.*` event. It must not write anything but deletions.

use std::time::{Duration, Instant};

use rusqlite::{Connection, params};

use super::logged;
use crate::config::History;

/// Rows a statement deletes at most.
const BATCH: u64 = 2000;

pub(super) fn run(db: &Connection, now: u64, history: &History) {
    let before =
        |keep: Duration| now.saturating_sub(u64::try_from(keep.as_millis()).unwrap_or(u64::MAX));
    logged("prune samples", samples(db, before(history.samples)));
    logged(
        "prune host samples",
        batches(
            db,
            "DELETE FROM host_samples WHERE at IN (SELECT at FROM host_samples WHERE at < ?1 LIMIT ?2)",
            before(history.samples),
        ),
    );
    logged("prune logs", logs(db, before(history.logs)));
    logged(
        "prune events",
        batches(
            db,
            // `announced` keeps each repository's latest: what a restarted daemon compares its config with to
            // announce a removal (`read::announced_repositories`).
            "DELETE FROM events WHERE id IN (SELECT id FROM events WHERE at < ?1 \
             AND id NOT IN (SELECT event FROM announced) LIMIT ?2)",
            before(history.events),
        ),
    );
    logged(
        "prune cache sizes",
        db.execute(
            "DELETE FROM cache_sizes WHERE at < ?1",
            [before(history.events)],
        ),
    );
}

/// Runs `delete` until it deletes nothing, pausing after each run as long as it took.
fn batched(mut delete: impl FnMut() -> rusqlite::Result<usize>) -> rusqlite::Result<()> {
    loop {
        let started = Instant::now();
        if delete()? == 0 {
            return Ok(());
        }
        std::thread::sleep(started.elapsed());
    }
}

/// Runs `sql`, which deletes up to `?2` rows older than `?1`, until it deletes none.
fn batches(db: &Connection, sql: &str, before: u64) -> rusqlite::Result<()> {
    let mut delete = db.prepare(sql)?;
    batched(|| delete.execute(params![before, BATCH]))
}

/// The runners whose samples reach back before `before`, read first: deleting by runner and time uses the primary
/// key.
fn samples(db: &Connection, before: u64) -> rusqlite::Result<()> {
    let runners: Vec<String> = db
        .prepare(
            "SELECT name FROM runners WHERE started_at < ?1 \
             AND EXISTS (SELECT 1 FROM samples WHERE runner = name AND at < ?1)",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut delete = db.prepare(
        "DELETE FROM samples WHERE runner = ?1 AND at IN \
         (SELECT at FROM samples WHERE runner = ?1 AND at < ?2 LIMIT ?3)",
    )?;
    for runner in runners {
        batched(|| delete.execute(params![runner, before, BATCH]))?;
    }
    Ok(())
}

/// The log lines of jobs that finished before `before`.
fn logs(db: &Connection, before: u64) -> rusqlite::Result<()> {
    let jobs: Vec<u64> = db
        .prepare(
            "SELECT id FROM jobs WHERE finished_at < ?1 AND state IN ('completed', 'lost', 'elsewhere') \
             AND EXISTS (SELECT 1 FROM log_lines WHERE job = id)",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut delete = db.prepare(
        "DELETE FROM log_lines WHERE job = ?1 AND line IN \
         (SELECT line FROM log_lines WHERE job = ?1 LIMIT ?2)",
    )?;
    for job in jobs {
        batched(|| delete.execute(params![job, BATCH]))?;
    }
    Ok(())
}
