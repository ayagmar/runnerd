//! Pruning: deletes what is older than `[history]` keeps, on its own connection off the writer thread, a batch per
//! transaction with a pause after each as long as the batch took, so the writer's transactions get the lock in
//! between. It stops between two batches once the store is closed. It keeps each repository's latest
//! `repository.*` event, and jobs that are not final. It must not write anything but deletions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::logged;
use crate::config::History;

/// Rows a statement deletes at most.
const BATCH: u64 = 2000;

/// The pruning connection, and the flag that stops a prune between two batches.
pub(super) struct Db<'prune> {
    pub(super) connection: &'prune Connection,
    pub(super) stop: &'prune AtomicBool,
}

impl Db<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

impl std::ops::Deref for Db<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection
    }
}

pub(super) fn run(db: &Db<'_>, now: u64, history: &History) {
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
    logged("prune jobs", jobs(db, before(history.jobs)));
}

/// Runs `delete` until it deletes nothing or the prune is stopped, pausing after each run as long as it took.
fn batched(
    db: &Db<'_>,
    mut delete: impl FnMut() -> rusqlite::Result<usize>,
) -> rusqlite::Result<()> {
    loop {
        if db.stopped() {
            return Ok(());
        }
        let started = Instant::now();
        if delete()? == 0 {
            return Ok(());
        }
        std::thread::sleep(started.elapsed());
    }
}

/// Runs `sql`, which deletes up to `?2` rows older than `?1`, until it deletes none.
fn batches(db: &Db<'_>, sql: &str, before: u64) -> rusqlite::Result<()> {
    let mut delete = db.prepare(sql)?;
    batched(db, || delete.execute(params![before, BATCH]))
}

/// The runners whose samples reach back before `before`, read first: deleting by runner and time uses the primary
/// key.
fn samples(db: &Db<'_>, before: u64) -> rusqlite::Result<()> {
    let runners: Vec<String> = db
        .prepare(
            "SELECT name FROM runners WHERE started_at < ?1 \
             AND EXISTS (SELECT 1 FROM samples WHERE runner = name AND at < ?1)",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    delete_samples(db, &runners, before)
}

fn delete_samples(db: &Db<'_>, runners: &[String], before: u64) -> rusqlite::Result<()> {
    let mut delete = db.prepare(
        "DELETE FROM samples WHERE runner = ?1 AND at IN \
         (SELECT at FROM samples WHERE runner = ?1 AND at < ?2 LIMIT ?3)",
    )?;
    for runner in runners {
        batched(db, || delete.execute(params![runner, before, BATCH]))?;
    }
    Ok(())
}

/// The log lines of jobs that finished before `before`.
fn logs(db: &Db<'_>, before: u64) -> rusqlite::Result<()> {
    let jobs: Vec<u64> = db
        .prepare(
            "SELECT id FROM jobs WHERE finished_at < ?1 AND state IN ('completed', 'lost', 'elsewhere') \
             AND EXISTS (SELECT 1 FROM log_lines WHERE job = id)",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    delete_logs(db, &jobs)
}

fn delete_logs(db: &Db<'_>, jobs: &[u64]) -> rusqlite::Result<()> {
    let mut delete = db.prepare(
        "DELETE FROM log_lines WHERE job = ?1 AND line IN \
         (SELECT line FROM log_lines WHERE job = ?1 LIMIT ?2)",
    )?;
    for job in jobs {
        batched(db, || delete.execute(params![job, BATCH]))?;
    }
    Ok(())
}

/// The records of jobs that ended before `before`, with their steps (and log, if `history.logs` is longer), then
/// the runners removed before it that no job names, with their samples.
fn jobs(db: &Db<'_>, before: u64) -> rusqlite::Result<()> {
    let jobs: Vec<u64> = db
        .prepare(
            "SELECT id FROM jobs WHERE coalesce(finished_at, queued_at) < ?1 \
             AND state IN ('completed', 'lost', 'elsewhere')",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    delete_logs(db, &jobs)?;
    let mut steps = db.prepare("DELETE FROM steps WHERE job = ?1")?;
    let mut records = db.prepare("DELETE FROM jobs WHERE id = ?1")?;
    for chunk in jobs.chunks(usize::try_from(BATCH).unwrap_or(usize::MAX)) {
        batched_once(db, || {
            for job in chunk {
                steps.execute([job])?;
                records.execute([job])?;
            }
            Ok(())
        })?;
    }
    let runners: Vec<String> = db
        .prepare(
            "SELECT name FROM runners WHERE coalesce(removed_at, started_at) < ?1 \
             AND name NOT IN (SELECT runner FROM jobs WHERE runner IS NOT NULL)",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // Taken while they ran, so all before `before`.
    delete_samples(db, &runners, before)?;
    let mut delete = db.prepare("DELETE FROM runners WHERE name = ?1")?;
    for chunk in runners.chunks(usize::try_from(BATCH).unwrap_or(usize::MAX)) {
        batched_once(db, || {
            for runner in chunk {
                delete.execute([runner])?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Runs `delete` in one transaction, then pauses as long as it took; nothing once the prune is stopped.
fn batched_once(
    db: &Db<'_>,
    delete: impl FnOnce() -> rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    if db.stopped() {
        return Ok(());
    }
    let started = Instant::now();
    let transaction = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    delete()?;
    transaction.commit()?;
    std::thread::sleep(started.elapsed());
    Ok(())
}
