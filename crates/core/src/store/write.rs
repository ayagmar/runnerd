//! The writer thread: applies writes in the order they were sent, closes job records once both GitHub and the
//! engine are done with them, and announces what changed. A change and the durable events it announces commit in
//! one transaction and are broadcast only after, so a subscriber that reads the stored events after subscribing
//! misses none, and a crash never keeps a change without its event.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tokio::sync::{broadcast, mpsc};

use super::read::{self, secs};
use super::{LogPosition, MirrorPosition, RunnerRecord, SampleRecord, Write, logged};
use crate::api::{self, Event, EventEnvelope};
use crate::github::{JobDetails, JobStep, QueuedJob};
use crate::time;

pub(super) fn run(
    db: Connection,
    mut queue: mpsc::UnboundedReceiver<Write>,
    events: &broadcast::Sender<Arc<EventEnvelope>>,
) {
    let writer = Writer { db, events };
    while let Some(write) = queue.blocking_recv() {
        writer.apply(write);
    }
}

struct Writer<'a> {
    db: Connection,
    events: &'a broadcast::Sender<Arc<EventEnvelope>>,
}

/// The events a change announces once it commits.
type Announced = Vec<Event>;

fn timestamp(text: Option<&str>) -> Option<u64> {
    text.and_then(time::parse_ms)
}

impl Writer<'_> {
    fn apply(&self, write: Write) {
        match write {
            Write::Queued { repository, job } => {
                self.change("record a queued job", |announced| {
                    self.queued(&repository, &job, announced)
                });
            }
            Write::RunnerStarted(runner) => self.runner_started(&runner),
            Write::RunnerRemoved { runner, at } => {
                self.change("record a removed runner", |announced| {
                    self.runner_removed(&runner, at, announced)
                });
            }
            Write::Claimed {
                job,
                run_id,
                runner,
                at,
            } => {
                self.change("record a claimed job", |announced| {
                    self.claimed(job, run_id, &runner, at, announced)
                });
            }
            Write::Details { details } => {
                self.change("record a job's progress", |announced| {
                    self.details(&details, announced)
                });
            }
            Write::Elsewhere { details } => {
                self.change("close a job that ran elsewhere", |announced| {
                    self.elsewhere(&details, announced)
                });
            }
            Write::Lost { job, at } => {
                self.change("close a lost job", |announced| {
                    self.close(job, "lost", Some(at), announced)
                });
            }
            Write::Log {
                job,
                lines,
                position,
                max_bytes,
                _backlog,
            } => {
                self.change("store log lines", |announced| {
                    self.log(job, lines, position, max_bytes, announced)
                });
            }
            Write::Samples(samples) => {
                self.change("record samples", |announced| {
                    self.samples(&samples, announced)
                });
            }
            Write::HostSample(sample) => {
                self.change("record a host sample", |announced| {
                    self.db.execute(
                        "INSERT OR REPLACE INTO host_samples (at, cpu, memory) VALUES (?1, ?2, ?3)",
                        params![sample.at * 1000, sample.cpu, sample.memory],
                    )?;
                    announced.push(Event::HostSample { sample });
                    Ok(())
                });
            }
            Write::MirrorBytes { bytes, until } => self.mirror_bytes(&bytes, until),
            Write::CacheSizes { at, sizes } => {
                for (name, bytes) in sizes {
                    logged(
                        "record a cache size",
                        self.db.execute(
                            "INSERT OR REPLACE INTO cache_sizes (at, name, bytes) VALUES (?1, ?2, ?3)",
                            params![at, name, bytes],
                        ),
                    );
                }
            }
            Write::Event(event) => {
                self.change("store an event", |announced| {
                    announced.push(event);
                    Ok(())
                });
            }
            Write::Prune { now, history } => self.prune(now, &history),
            Write::Flush(done) => {
                let _ = done.send(());
            }
        }
    }

    fn runner_started(&self, runner: &RunnerRecord) {
        logged(
            "record a runner",
            self.db.execute(
                "INSERT OR IGNORE INTO runners (name, repository, profile, slot, container, started_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    runner.name,
                    runner.repository,
                    runner.profile,
                    runner.slot,
                    runner.container,
                    runner.started_at
                ],
            ),
        );
    }

    /// Makes a change and stores the durable events it announces in one transaction, then sends them, with their
    /// IDs, to every subscriber. On any error nothing is kept or sent.
    fn change(&self, what: &str, change: impl FnOnce(&mut Announced) -> rusqlite::Result<()>) {
        let result = (|| {
            // Immediate, so a busy database is waited for: a deferred transaction that read first fails at once.
            let transaction = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            let mut announced = Vec::new();
            change(&mut announced)?;
            let envelopes = announced
                .into_iter()
                .map(|event| self.store_event(event))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            transaction.commit()?;
            Ok(envelopes)
        })();
        for envelope in logged(what, result).unwrap_or_default() {
            // No subscriber is not an error.
            let _ = self.events.send(Arc::new(envelope));
        }
    }

    /// Stores an event if it is durable, giving it its ID.
    fn store_event(&self, event: Event) -> rusqlite::Result<EventEnvelope> {
        let at = time::now_ms();
        let id = if event.is_durable() {
            let body = serde_json::to_string(&event)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            self.db.execute(
                "INSERT INTO events (at, body) VALUES (?1, ?2)",
                params![at, body],
            )?;
            u64::try_from(self.db.last_insert_rowid()).ok()
        } else {
            None
        };
        Ok(EventEnvelope {
            id,
            at: secs(at),
            event,
        })
    }

    fn queued(
        &self,
        repository: &str,
        job: &QueuedJob,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        let labels = serde_json::to_string(&job.labels).unwrap_or_else(|_| "[]".to_owned());
        let queued_at = if job.created_at == 0 {
            time::now_ms()
        } else {
            job.created_at
        };
        let inserted = self.db.execute(
            "INSERT INTO jobs (id, repository, run_id, run_attempt, workflow, name, branch, url, labels, state, \
             queued_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10) ON CONFLICT (id) DO NOTHING",
            params![
                job.id,
                repository,
                job.run_id,
                job.run_attempt,
                job.workflow,
                job.name,
                job.branch,
                job.url,
                labels,
                queued_at
            ],
        )?;
        if inserted == 1
            && let Some(job) = read::summary(&self.db, job.id)?
        {
            announced.push(Event::JobQueued { job });
        }
        Ok(())
    }

    fn claimed(
        &self,
        job: u64,
        run_id: u64,
        runner: &str,
        at: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        // A job the daemon never saw queued: GitHub's listing lagged, or it was queued before the daemon started.
        let inserted = self.db.execute(
            "INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, runner, queued_at, \
             started_at) SELECT ?1, repository, ?2, 1, '', '[]', 'running', name, ?3, ?3 FROM runners \
             WHERE name = ?4 ON CONFLICT (id) DO NOTHING",
            params![job, run_id, at, runner],
        )?;
        let updated = self.db.execute(
            "UPDATE jobs SET state = 'running', runner = ?2, started_at = coalesce(started_at, ?3) \
             WHERE id = ?1 AND state = 'queued'",
            params![job, runner, at],
        )?;
        if (inserted == 1 || updated == 1)
            && let Some(job) = read::summary(&self.db, job)?
        {
            announced.push(Event::JobStarted { job });
        }
        Ok(())
    }

    fn details(&self, details: &JobDetails, announced: &mut Announced) -> rusqlite::Result<()> {
        let completed = details.status == "completed";
        let labels = serde_json::to_string(&details.labels).unwrap_or_else(|_| "[]".to_owned());
        let updated = self.db.execute(
            "UPDATE jobs SET run_id = ?2, run_attempt = ?3, name = ?4, workflow = ?5, branch = ?6, url = ?7, \
             labels = ?8, queued_at = coalesce(?9, queued_at), started_at = coalesce(?10, started_at), \
             finished_at = CASE WHEN ?11 THEN coalesce(?12, finished_at) ELSE finished_at END, \
             conclusion = ?13, github_completed = ?11 WHERE id = ?1 AND state = 'running'",
            params![
                details.id,
                details.run_id,
                details.run_attempt,
                details.name,
                details.workflow_name,
                details.head_branch,
                details.html_url,
                labels,
                timestamp(details.created_at.as_deref()),
                timestamp(details.started_at.as_deref()),
                completed,
                timestamp(details.completed_at.as_deref()),
                details.conclusion,
            ],
        )?;
        if updated != 1 {
            return Ok(());
        }
        for step in &details.steps {
            if let Some(step) = self.step(details.id, step)? {
                announced.push(Event::JobStep {
                    job: details.id,
                    step,
                });
            }
        }
        self.close_if_done(details.id, announced)
    }

    /// Stores a step; returns it if it is new or changed.
    fn step(&self, job: u64, step: &JobStep) -> rusqlite::Result<Option<api::Step>> {
        let stored = api::Step {
            number: step.number,
            name: step.name.clone(),
            status: step.status.clone(),
            conclusion: step.conclusion.clone(),
            started_at: timestamp(step.started_at.as_deref()).map(secs),
            completed_at: timestamp(step.completed_at.as_deref()).map(secs),
        };
        let changed = self.db.execute(
            "INSERT INTO steps (job, number, name, status, conclusion, started_at, completed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (job, number) DO UPDATE SET \
             name = excluded.name, status = excluded.status, conclusion = excluded.conclusion, \
             started_at = excluded.started_at, completed_at = excluded.completed_at \
             WHERE (name, status, conclusion, started_at, completed_at) IS NOT \
             (excluded.name, excluded.status, excluded.conclusion, excluded.started_at, excluded.completed_at)",
            params![
                job,
                step.number,
                step.name,
                step.status,
                step.conclusion,
                timestamp(step.started_at.as_deref()),
                timestamp(step.completed_at.as_deref()),
            ],
        )?;
        Ok((changed == 1).then_some(stored))
    }

    fn runner_removed(
        &self,
        runner: &str,
        at: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE runners SET removed_at = ?2 WHERE name = ?1 AND removed_at IS NULL",
            params![runner, at],
        )?;
        let job: Option<u64> = self
            .db
            .query_row(
                "SELECT id FROM jobs WHERE runner = ?1 AND state = 'running'",
                [runner],
                |row| row.get(0),
            )
            .optional()?;
        match job {
            Some(job) => self.close_if_done(job, announced),
            None => Ok(()),
        }
    }

    /// Closes a running job once GitHub reports it completed and its runner is removed.
    fn close_if_done(&self, job: u64, announced: &mut Announced) -> rusqlite::Result<()> {
        let done = self
            .db
            .query_row(
                "SELECT j.github_completed AND r.removed_at IS NOT NULL FROM jobs j \
                 JOIN runners r ON r.name = j.runner WHERE j.id = ?1 AND j.state = 'running'",
                [job],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        if done == Some(true) {
            self.close(job, "completed", None, announced)?;
        }
        Ok(())
    }

    /// Keeps a job's metrics and makes its record final.
    fn close(
        &self,
        job: u64,
        state: &str,
        finished_at: Option<u64>,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        let metrics = read::metrics(&self.db, job)?;
        let closed = self.db.execute(
            "UPDATE jobs SET state = ?2, finished_at = coalesce(finished_at, ?3), cpu_time = ?4, \
             cpu_peak = ?5, cpu_average = ?6, memory_peak = ?7, memory_average = ?8, \
             network_received = ?9, network_sent = ?10 WHERE id = ?1 AND state = 'running'",
            params![
                job,
                state,
                finished_at,
                metrics.cpu_time,
                metrics.cpu_peak,
                metrics.cpu_average,
                metrics.memory_peak,
                metrics.memory_average,
                metrics.network_received,
                metrics.network_sent,
            ],
        )?;
        if closed == 1
            && let Some(job) = read::summary(&self.db, job)?
        {
            announced.push(Event::JobFinished { job });
        }
        Ok(())
    }

    fn elsewhere(&self, details: &JobDetails, announced: &mut Announced) -> rusqlite::Result<()> {
        let closed = self.db.execute(
            "UPDATE jobs SET state = 'elsewhere', conclusion = ?2, started_at = ?3, finished_at = ?4 \
             WHERE id = ?1 AND state = 'queued'",
            params![
                details.id,
                details.conclusion,
                timestamp(details.started_at.as_deref()),
                timestamp(details.completed_at.as_deref()),
            ],
        )?;
        if closed == 1
            && let Some(job) = read::summary(&self.db, details.id)?
        {
            announced.push(Event::JobFinished { job });
        }
        Ok(())
    }

    fn log(
        &self,
        job: u64,
        lines: Vec<String>,
        position: Option<LogPosition>,
        max_bytes: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        let (mut count, mut bytes, mut truncated): (u64, u64, bool) = self.db.query_row(
            "SELECT log_lines, log_bytes, log_truncated FROM jobs WHERE id = ?1",
            [job],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        for line in lines {
            if truncated {
                break;
            }
            let size = line.len() as u64 + 1;
            if bytes + size > max_bytes {
                truncated = true;
                break;
            }
            self.db.execute(
                "INSERT OR IGNORE INTO log_lines (job, line, text) VALUES (?1, ?2, ?3)",
                params![job, count, line],
            )?;
            announced.push(Event::LogLine {
                job,
                line: count,
                text: line,
            });
            count += 1;
            bytes += size;
        }
        self.db.execute(
            "UPDATE jobs SET log_lines = ?2, log_bytes = ?3, log_truncated = ?4, \
             log_page = coalesce(?5, log_page), log_offset = coalesce(?6, log_offset) WHERE id = ?1",
            params![
                job,
                count,
                bytes,
                truncated,
                position.map(|position| position.page),
                position.map(|position| position.offset)
            ],
        )?;
        Ok(())
    }

    fn samples(&self, samples: &[SampleRecord], announced: &mut Announced) -> rusqlite::Result<()> {
        for sample in samples {
            self.db.execute(
                "INSERT OR REPLACE INTO samples (runner, at, cpu, cpu_usage, memory, network_received, \
                 network_sent) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    sample.runner,
                    sample.at,
                    sample.cpu,
                    sample.cpu_usage,
                    sample.memory,
                    sample.network_received,
                    sample.network_sent
                ],
            )?;
            let job: Option<u64> = self
                .db
                .query_row(
                    "SELECT id FROM jobs WHERE runner = ?1 AND state = 'running'",
                    [&sample.runner],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(job) = job {
                announced.push(Event::JobSample {
                    job,
                    sample: api::Sample {
                        at: secs(sample.at),
                        cpu: sample.cpu,
                        memory: sample.memory,
                        network_received: sample.network_received,
                        network_sent: sample.network_sent,
                    },
                });
            }
        }
        Ok(())
    }

    /// Counts a batch of the mirror's log and where it ends, together: counting part of it, or counting it without
    /// moving past it, would count it again after a restart.
    fn mirror_bytes(&self, bytes: &[(String, u64)], until: MirrorPosition) {
        self.change("count mirror bytes", |_| {
            for (runner, bytes) in bytes {
                self.db.execute(
                    "UPDATE runners SET mirror_bytes = mirror_bytes + ?2 WHERE name = ?1",
                    params![runner, bytes],
                )?;
            }
            self.db.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('mirror_log_until', ?1), ('mirror_log_seen', ?2)",
                [until.at, until.seen],
            )?;
            Ok(())
        });
    }

    fn prune(&self, now: u64, history: &crate::config::History) {
        let before = |keep: std::time::Duration| {
            now.saturating_sub(u64::try_from(keep.as_millis()).unwrap_or(u64::MAX))
        };
        for (what, sql, before) in [
            (
                "prune samples",
                "DELETE FROM samples WHERE at < ?1",
                before(history.samples),
            ),
            (
                "prune host samples",
                "DELETE FROM host_samples WHERE at < ?1",
                before(history.samples),
            ),
            (
                "prune logs",
                "DELETE FROM log_lines WHERE job IN (SELECT id FROM jobs WHERE finished_at < ?1 \
                 AND state IN ('completed', 'lost', 'elsewhere'))",
                before(history.logs),
            ),
            (
                "prune events",
                "DELETE FROM events WHERE at < ?1",
                before(history.events),
            ),
            (
                "prune cache sizes",
                "DELETE FROM cache_sizes WHERE at < ?1",
                before(history.events),
            ),
        ] {
            logged(what, self.db.execute(sql, [before]));
        }
    }
}
