//! The writer thread: applies writes in the order they were sent, closes job records once both GitHub and the
//! engine are done with them, and announces what changed. A durable event is stored before it is broadcast, so a
//! subscriber that reads the stored events after subscribing misses none.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{broadcast, mpsc};

use super::read::{self, secs};
use super::{LogPosition, SampleRecord, Write, logged};
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

fn timestamp(text: Option<&str>) -> Option<u64> {
    text.and_then(time::parse_ms)
}

impl Writer<'_> {
    fn apply(&self, write: Write) {
        match write {
            Write::Queued { repository, job } => self.queued(&repository, &job),
            Write::RunnerStarted(runner) => {
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
            Write::RunnerRemoved { runner, at } => self.runner_removed(&runner, at),
            Write::Claimed {
                job,
                run_id,
                runner,
                at,
            } => self.claimed(job, run_id, &runner, at),
            Write::Details { details } => self.details(&details),
            Write::Elsewhere { details } => self.elsewhere(&details),
            Write::Lost { job, at } => self.lost(job, at),
            Write::Log {
                job,
                lines,
                position,
                max_bytes,
            } => self.log(job, lines, position, max_bytes),
            Write::Samples(samples) => self.samples(&samples),
            Write::HostSample(sample) => {
                logged(
                    "record a host sample",
                    self.db.execute(
                        "INSERT OR REPLACE INTO host_samples (at, cpu, memory) VALUES (?1, ?2, ?3)",
                        params![sample.at * 1000, sample.cpu, sample.memory],
                    ),
                );
                self.emit(Event::HostSample { sample });
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
            Write::Event(event) => self.emit(event),
            Write::Prune { now, history } => self.prune(now, &history),
            Write::Flush(done) => {
                let _ = done.send(());
            }
        }
    }

    /// Stores a durable event, then sends it, with its ID, to every subscriber.
    fn emit(&self, event: Event) {
        let at = time::now_ms();
        let id = if event.is_durable() {
            serde_json::to_string(&event).ok().and_then(|body| {
                logged(
                    "store an event",
                    self.db
                        .execute(
                            "INSERT INTO events (at, body) VALUES (?1, ?2)",
                            params![at, body],
                        )
                        .map(|_| self.db.last_insert_rowid()),
                )
                .and_then(|id| u64::try_from(id).ok())
            })
        } else {
            None
        };
        // No subscriber is not an error.
        let _ = self.events.send(Arc::new(EventEnvelope {
            id,
            at: secs(at),
            event,
        }));
    }

    fn summary(&self, job: u64) -> Option<api::JobSummary> {
        logged("read a job", read::summary(&self.db, job)).flatten()
    }

    fn queued(&self, repository: &str, job: &QueuedJob) {
        let labels = serde_json::to_string(&job.labels).unwrap_or_else(|_| "[]".to_owned());
        let queued_at = if job.created_at == 0 {
            time::now_ms()
        } else {
            job.created_at
        };
        let inserted = logged(
            "record a queued job",
            self.db.execute(
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
            ),
        );
        if inserted == Some(1)
            && let Some(job) = self.summary(job.id)
        {
            self.emit(Event::JobQueued { job });
        }
    }

    fn claimed(&self, job: u64, run_id: u64, runner: &str, at: u64) {
        // A job the daemon never saw queued: GitHub's listing lagged, or it was queued before the daemon started.
        let inserted = logged(
            "record a claimed job",
            self.db.execute(
                "INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, runner, queued_at, \
                 started_at) SELECT ?1, repository, ?2, 1, '', '[]', 'running', name, ?3, ?3 FROM runners \
                 WHERE name = ?4 ON CONFLICT (id) DO NOTHING",
                params![job, run_id, at, runner],
            ),
        );
        let updated = logged(
            "record a claimed job",
            self.db.execute(
                "UPDATE jobs SET state = 'running', runner = ?2, started_at = coalesce(started_at, ?3) \
                 WHERE id = ?1 AND state = 'queued'",
                params![job, runner, at],
            ),
        );
        if (inserted == Some(1) || updated == Some(1))
            && let Some(job) = self.summary(job)
        {
            self.emit(Event::JobStarted { job });
        }
    }

    fn details(&self, details: &JobDetails) {
        let completed = details.status == "completed";
        let labels = serde_json::to_string(&details.labels).unwrap_or_else(|_| "[]".to_owned());
        let updated = logged(
            "record a job's progress",
            self.db.execute(
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
            ),
        );
        if updated != Some(1) {
            return;
        }
        for step in &details.steps {
            if let Some(step) = self.step(details.id, step) {
                self.emit(Event::JobStep {
                    job: details.id,
                    step,
                });
            }
        }
        self.close_if_done(details.id);
    }

    /// Stores a step; returns it if it is new or changed.
    fn step(&self, job: u64, step: &JobStep) -> Option<api::Step> {
        let stored = api::Step {
            number: step.number,
            name: step.name.clone(),
            status: step.status.clone(),
            conclusion: step.conclusion.clone(),
            started_at: timestamp(step.started_at.as_deref()).map(secs),
            completed_at: timestamp(step.completed_at.as_deref()).map(secs),
        };
        let changed = logged(
            "record a step",
            self.db.execute(
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
            ),
        );
        (changed == Some(1)).then_some(stored)
    }

    fn runner_removed(&self, runner: &str, at: u64) {
        logged(
            "record a removed runner",
            self.db.execute(
                "UPDATE runners SET removed_at = ?2 WHERE name = ?1 AND removed_at IS NULL",
                params![runner, at],
            ),
        );
        let job: Option<u64> = logged(
            "read a runner's job",
            self.db
                .query_row(
                    "SELECT id FROM jobs WHERE runner = ?1 AND state = 'running'",
                    [runner],
                    |row| row.get(0),
                )
                .optional(),
        )
        .flatten();
        if let Some(job) = job {
            self.close_if_done(job);
        }
    }

    /// Closes a running job once GitHub reports it completed and its runner is removed.
    fn close_if_done(&self, job: u64) {
        let done = logged(
            "read a job's progress",
            self.db
                .query_row(
                    "SELECT j.github_completed AND r.removed_at IS NOT NULL FROM jobs j \
                     JOIN runners r ON r.name = j.runner WHERE j.id = ?1 AND j.state = 'running'",
                    [job],
                    |row| row.get::<_, bool>(0),
                )
                .optional(),
        )
        .flatten();
        if done == Some(true) {
            self.close(job, "completed", None);
        }
    }

    /// Keeps a job's metrics and makes its record final.
    fn close(&self, job: u64, state: &str, finished_at: Option<u64>) {
        let Some(metrics) = logged("compute a job's metrics", read::metrics(&self.db, job)) else {
            return;
        };
        let closed = logged(
            "close a job",
            self.db.execute(
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
            ),
        );
        if closed == Some(1)
            && let Some(job) = self.summary(job)
        {
            self.emit(Event::JobFinished { job });
        }
    }

    fn elsewhere(&self, details: &JobDetails) {
        let closed = logged(
            "close a job that ran elsewhere",
            self.db.execute(
                "UPDATE jobs SET state = 'elsewhere', conclusion = ?2, started_at = ?3, finished_at = ?4 \
                 WHERE id = ?1 AND state = 'queued'",
                params![
                    details.id,
                    details.conclusion,
                    timestamp(details.started_at.as_deref()),
                    timestamp(details.completed_at.as_deref()),
                ],
            ),
        );
        if closed == Some(1)
            && let Some(job) = self.summary(details.id)
        {
            self.emit(Event::JobFinished { job });
        }
    }

    fn lost(&self, job: u64, at: u64) {
        self.close(job, "lost", Some(at));
    }

    fn log(&self, job: u64, lines: Vec<String>, position: LogPosition, max_bytes: u64) {
        let result = (|| -> rusqlite::Result<Vec<(u64, String)>> {
            let transaction = self.db.unchecked_transaction()?;
            let (mut count, mut bytes, mut truncated): (u64, u64, bool) = transaction.query_row(
                "SELECT log_lines, log_bytes, log_truncated FROM jobs WHERE id = ?1",
                [job],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let mut stored = Vec::new();
            for line in lines {
                if truncated {
                    break;
                }
                let size = line.len() as u64 + 1;
                if bytes + size > max_bytes {
                    truncated = true;
                    break;
                }
                transaction.execute(
                    "INSERT OR IGNORE INTO log_lines (job, line, text) VALUES (?1, ?2, ?3)",
                    params![job, count, line],
                )?;
                stored.push((count, line));
                count += 1;
                bytes += size;
            }
            transaction.execute(
                "UPDATE jobs SET log_lines = ?2, log_bytes = ?3, log_truncated = ?4, log_page = ?5, \
                 log_offset = ?6 WHERE id = ?1",
                params![job, count, bytes, truncated, position.page, position.offset],
            )?;
            transaction.commit()?;
            Ok(stored)
        })();
        for (line, text) in logged("store log lines", result).unwrap_or_default() {
            self.emit(Event::LogLine { job, line, text });
        }
    }

    fn samples(&self, samples: &[SampleRecord]) {
        for sample in samples {
            logged(
                "record a sample",
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
                ),
            );
            let job: Option<u64> = logged(
                "read a runner's job",
                self.db
                    .query_row(
                        "SELECT id FROM jobs WHERE runner = ?1 AND state = 'running'",
                        [&sample.runner],
                        |row| row.get(0),
                    )
                    .optional(),
            )
            .flatten();
            if let Some(job) = job {
                self.emit(Event::JobSample {
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
    }

    fn mirror_bytes(&self, bytes: &[(String, u64)], until: u64) {
        for (runner, bytes) in bytes {
            logged(
                "count mirror bytes",
                self.db.execute(
                    "UPDATE runners SET mirror_bytes = mirror_bytes + ?2 WHERE name = ?1",
                    params![runner, bytes],
                ),
            );
        }
        logged(
            "record the mirror log's position",
            self.db.execute(
                "INSERT INTO meta (key, value) VALUES ('mirror_log_until', ?1) \
                 ON CONFLICT (key) DO UPDATE SET value = max(value, excluded.value)",
                [until],
            ),
        );
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
