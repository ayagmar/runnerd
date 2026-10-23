//! Queries: job records with their timelines and metrics, samples, logs, events, stats, cache sizes and paused
//! repositories, turned into `api` types. Milliseconds in the database become seconds in the API. Shared by the
//! API's reads and by the writer, which reads back what it announces.

use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use tracing::warn;

use super::{LogPosition, OpenJob};
use crate::api::{self, EventEnvelope, JobState};
use crate::sizing;

const DEFAULT_PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;
const DAY_MS: u64 = 24 * 3600 * 1000;

/// The most buckets `stats` counts, before `until` or now: 31 days of hours, 4,464 rows for six repositories.
/// Sixty days of hours were 2 MB of JSON.
const STATS_BUCKETS: u64 = 31 * 24;

pub(super) fn secs(ms: u64) -> u64 {
    ms / 1000
}

pub(super) fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Completed => "completed",
        JobState::Lost => "lost",
        JobState::Elsewhere => "elsewhere",
    }
}

fn state(name: &str) -> JobState {
    match name {
        "running" => JobState::Running,
        "completed" => JobState::Completed,
        "lost" => JobState::Lost,
        "elsewhere" => JobState::Elsewhere,
        _ => JobState::Queued,
    }
}

/// The columns `summary_from` reads, in its order.
const SUMMARY: &str = "j.id, j.repository, j.run_id, j.run_attempt, j.workflow, j.name, j.branch, j.url, j.labels, \
     j.state, j.conclusion, j.runner, r.profile, r.slot, j.queued_at, r.started_at, j.started_at, j.finished_at, \
     r.removed_at, j.cpu_time, j.cpu_peak, j.cpu_average, j.memory_peak, j.memory_average, j.network_received, \
     j.network_sent, coalesce(r.mirror_bytes, 0), j.log_lines, j.log_bytes, j.log_truncated, \
     r.memory_cap, r.cpu_cap \
     FROM jobs j LEFT JOIN runners r ON r.name = j.runner";

fn summary_from(row: &Row<'_>) -> rusqlite::Result<api::JobSummary> {
    let labels: String = row.get(8)?;
    let runner: Option<String> = row.get(11)?;
    let profile: Option<String> = row.get(12)?;
    let slot: Option<usize> = row.get(13)?;
    let seconds = |index: usize| row.get::<_, Option<u64>>(index).map(|ms| ms.map(secs));
    Ok(api::JobSummary {
        id: row.get(0)?,
        repository: row.get(1)?,
        run_id: row.get(2)?,
        run_attempt: row.get(3)?,
        workflow: row.get(4)?,
        name: row.get(5)?,
        branch: row.get(6)?,
        url: row.get(7)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        state: state(&row.get::<_, String>(9)?),
        conclusion: row.get(10)?,
        runner: match (runner, profile, slot) {
            (Some(name), Some(profile), Some(slot)) => Some(api::JobRunner {
                name,
                profile,
                slot,
                memory_cap: row.get(30)?,
                cpu_cap: row.get(31)?,
            }),
            _ => None,
        },
        timeline: api::Timeline {
            queued_at: secs(row.get(14)?),
            container_started_at: seconds(15)?,
            started_at: seconds(16)?,
            finished_at: seconds(17)?,
            cleaned_at: seconds(18)?,
        },
        metrics: api::JobMetrics {
            cpu_time: row.get(19)?,
            cpu_peak: row.get(20)?,
            cpu_average: row.get(21)?,
            memory_peak: row.get(22)?,
            memory_average: row.get(23)?,
            network_received: row.get(24)?,
            network_sent: row.get(25)?,
            mirror_bytes: row.get(26)?,
        },
        log: api::LogSummary {
            lines: row.get(27)?,
            bytes: row.get(28)?,
            truncated: row.get(29)?,
        },
    })
}

/// A job without its steps. Before a record is final, its metrics come from its samples so far.
pub(super) fn summary(db: &Connection, id: u64) -> rusqlite::Result<Option<api::JobSummary>> {
    let Some(mut summary) = db
        .query_row(
            &format!("SELECT {SUMMARY} WHERE j.id = ?1"),
            [id],
            summary_from,
        )
        .optional()?
    else {
        return Ok(None);
    };
    if !summary.state.is_final() {
        let mirror_bytes = summary.metrics.mirror_bytes;
        summary.metrics = metrics(db, id)?;
        summary.metrics.mirror_bytes = mirror_bytes;
    }
    Ok(Some(summary))
}

/// A job's use over its samples: from its start to its end when known, else over its container's whole life.
/// `mirror_bytes` is left 0.
pub(super) fn metrics(db: &Connection, job: u64) -> rusqlite::Result<api::JobMetrics> {
    let window: Option<(Option<String>, Option<u64>, Option<u64>)> = db
        .query_row(
            "SELECT runner, started_at, finished_at FROM jobs WHERE id = ?1",
            [job],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((Some(runner), started, finished)) = window else {
        return Ok(api::JobMetrics::default());
    };
    let (from, to) = (started.unwrap_or(0), finished.unwrap_or(u64::MAX));
    let mut metrics = window_metrics(db, &runner, from, to)?;
    // A job shorter than a sample interval: the container's whole life says more than nothing.
    if metrics.memory_peak.is_none() {
        metrics = window_metrics(db, &runner, 0, u64::MAX)?;
    }
    Ok(metrics)
}

/// Gauges (memory, CPU peak) over the samples in `[from, to]`. Counters (CPU time, network) from the last sample
/// before `from`, or from zero at the container's start, to the last sample in the window: a counter's first sample
/// in the window already holds what the job used before it.
fn window_metrics(
    db: &Connection,
    runner: &str,
    from: u64,
    to: u64,
) -> rusqlite::Result<api::JobMetrics> {
    let before = db
        .query_row(
            "SELECT at, cpu_usage, network_received, network_sent FROM samples WHERE runner = ?1 AND at < ?2 \
             ORDER BY at DESC LIMIT 1",
            params![runner, from],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let baseline: (u64, u64, u64, u64) = if let Some(before) = before {
        before
    } else {
        let started: Option<u64> = db
            .query_row(
                "SELECT started_at FROM runners WHERE name = ?1",
                [runner],
                |row| row.get(0),
            )
            .optional()?;
        (started.unwrap_or(from), 0, 0, 0)
    };
    let (since, usage_before, received_before, sent_before) = baseline;
    let to = i64::try_from(to).unwrap_or(i64::MAX);
    db.query_row(
        "SELECT max(memory), CAST(avg(memory) AS INTEGER), max(cpu), max(cpu_usage), max(at), \
         max(network_received), max(network_sent) FROM samples WHERE runner = ?1 AND at >= ?2 AND at <= ?3",
        params![runner, from, to],
        |row| {
            let usage = row
                .get::<_, Option<u64>>(3)?
                .map(|usage| usage.saturating_sub(usage_before));
            let wall = row
                .get::<_, Option<u64>>(4)?
                .map(|last| last.saturating_sub(since));
            Ok(api::JobMetrics {
                memory_peak: row.get(0)?,
                memory_average: row.get(1)?,
                cpu_peak: row.get(2)?,
                // Microseconds to milliseconds.
                cpu_time: usage.map(|usage| usage / 1000),
                // CPU microseconds per wall millisecond are millicores.
                cpu_average: usage.zip(wall).map(|(usage, wall)| usage.checked_div(wall).unwrap_or(0)),
                network_received: row
                    .get::<_, Option<u64>>(5)?
                    .map(|received| received.saturating_sub(received_before)),
                network_sent: row
                    .get::<_, Option<u64>>(6)?
                    .map(|sent| sent.saturating_sub(sent_before)),
                mirror_bytes: 0,
            })
        },
    )
}

pub(super) fn steps(db: &Connection, job: u64) -> rusqlite::Result<Vec<api::Step>> {
    let mut statement = db.prepare_cached(
        "SELECT number, name, status, conclusion, started_at, completed_at FROM steps WHERE job = ?1 \
         ORDER BY number",
    )?;
    statement
        .query_map([job], |row| {
            Ok(api::Step {
                number: row.get(0)?,
                name: row.get(1)?,
                status: row.get(2)?,
                conclusion: row.get(3)?,
                started_at: row.get::<_, Option<u64>>(4)?.map(secs),
                completed_at: row.get::<_, Option<u64>>(5)?.map(secs),
            })
        })?
        .collect()
}

pub(super) fn job(db: &Connection, id: u64) -> rusqlite::Result<Option<api::Job>> {
    let Some(summary) = summary(db, id)? else {
        return Ok(None);
    };
    let admission: Option<String> = db
        .query_row(
            "SELECT r.admission FROM jobs j JOIN runners r ON r.name = j.runner WHERE j.id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(Some(api::Job {
        steps: steps(db, id)?,
        record: record(db, id)?,
        admission: admission.and_then(|admission| serde_json::from_str(&admission).ok()),
        summary,
    }))
}

fn record(db: &Connection, job: u64) -> rusqlite::Result<Option<api::RecordNote>> {
    db.query_row(
        "SELECT created_at, bytes, skipped, removed_at, removed_by FROM records WHERE job = ?1",
        [job],
        |row| {
            let removed_by: Option<String> = row.get(4)?;
            Ok(api::RecordNote {
                created_at: row.get::<_, Option<u64>>(0)?.map(secs),
                bytes: row.get(1)?,
                skipped: row.get(2)?,
                removed_at: row.get::<_, Option<u64>>(3)?.map(secs),
                removed_by: removed_by
                    .and_then(|name| serde_json::from_value(serde_json::Value::String(name)).ok()),
            })
        },
    )
    .optional()
}

pub(super) fn jobs(db: &Connection, query: &api::JobQuery) -> rusqlite::Result<api::JobPage> {
    let mut conditions = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    let mut condition = |sql: &str, value: Value| {
        values.push(value);
        conditions.push(format!("{sql} ?{}", values.len()));
    };
    if let Some(repository) = &query.repository {
        condition("j.repository =", Value::Text(repository.clone()));
    }
    if let Some(state) = query.state {
        condition("j.state =", Value::Text(state_name(state).to_owned()));
    }
    if let Some(conclusion) = &query.conclusion {
        condition("j.conclusion =", Value::Text(conclusion.clone()));
    }
    let ms =
        |secs: u64| Value::Integer(i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX));
    if let Some(since) = query.since {
        condition("j.queued_at >=", ms(since));
    }
    if let Some(until) = query.until {
        condition("j.queued_at <", ms(until));
    }
    if let Some(before) = query.before {
        condition(
            "j.id <",
            Value::Integer(i64::try_from(before).unwrap_or(i64::MAX)),
        );
    }
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let filter = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let ids: Vec<u64> = db
        .prepare(&format!(
            "SELECT j.id FROM jobs j {filter} ORDER BY j.id DESC LIMIT {}",
            limit + 1
        ))?
        .query_map(params_from_iter(values), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let more = ids.len() > limit as usize;
    let mut jobs = Vec::with_capacity(ids.len());
    for id in ids.into_iter().take(limit as usize) {
        jobs.extend(summary(db, id)?);
    }
    let next = if more {
        jobs.last().map(|job| job.id)
    } else {
        None
    };
    Ok(api::JobPage { jobs, next })
}

pub(super) fn job_samples(
    db: &Connection,
    job: u64,
    interval: Duration,
) -> rusqlite::Result<Option<api::SampleSeries>> {
    let Some(runner) = db
        .query_row("SELECT runner FROM jobs WHERE id = ?1", [job], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
    else {
        return Ok(None);
    };
    let samples = match runner {
        None => Vec::new(),
        Some(runner) => db
            .prepare_cached(
                "SELECT at, cpu, memory, network_received, network_sent FROM samples WHERE runner = ?1 \
                 ORDER BY at",
            )?
            .query_map([runner], |row| {
                Ok(api::Sample {
                    at: secs(row.get(0)?),
                    cpu: row.get(1)?,
                    memory: row.get(2)?,
                    network_received: row.get(3)?,
                    network_sent: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?,
    };
    Ok(Some(api::SampleSeries {
        job,
        interval: interval.as_secs(),
        samples,
    }))
}

pub(super) fn log_lines(
    db: &Connection,
    job: u64,
    from: u64,
    limit: u64,
) -> rusqlite::Result<Vec<(u64, String)>> {
    db.prepare_cached(
        "SELECT line, text FROM log_lines WHERE job = ?1 AND line >= ?2 ORDER BY line LIMIT ?3",
    )?
    .query_map(params![job, from, limit], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?
    .collect()
}

pub(super) fn events_after(
    db: &Connection,
    after: u64,
    limit: u64,
) -> rusqlite::Result<Vec<EventEnvelope>> {
    let rows: Vec<(u64, u64, String)> = db
        .prepare_cached("SELECT id, at, body FROM events WHERE id > ?1 ORDER BY id LIMIT ?2")?
        .query_map(params![after, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, at, body)| match serde_json::from_str(&body) {
            Ok(event) => Some(EventEnvelope {
                id: Some(id),
                at: secs(at),
                event,
            }),
            Err(error) => {
                warn!("job history: skipping event {id}, which does not parse: {error}");
                None
            }
        })
        .collect())
}

/// The repositories as each one's latest stored `repository.*` event announced them, by name.
pub(super) fn announced(db: &Connection) -> rusqlite::Result<super::Announced> {
    let bodies: Vec<String> = db
        .prepare_cached(
            "SELECT body FROM events WHERE id IN (SELECT event FROM announced) \
             OR id IN (SELECT event FROM announced_profiles) ORDER BY id",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut profiles = std::collections::BTreeMap::new();
    let mut repositories = std::collections::BTreeMap::new();
    for body in bodies {
        match serde_json::from_str(&body) {
            Ok(api::Event::ProfileChanged { profile }) => {
                profiles.insert(profile.name.clone(), profile);
            }
            Ok(api::Event::ProfileRemoved { profile }) => {
                profiles.remove(&profile);
            }
            Ok(api::Event::RepositoryChanged { repository }) => {
                repositories.insert(repository.name.clone(), repository);
            }
            Ok(api::Event::RepositoryRemoved { repository }) => {
                repositories.remove(&repository);
            }
            Ok(_) => {}
            // Skipping it would leave a repository or profile out, and a client resuming after it would keep that
            // one.
            Err(error) => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                ));
            }
        }
    }
    Ok(super::Announced {
        profiles: profiles.into_values().collect(),
        repositories: repositories.into_values().collect(),
    })
}

pub(super) fn paused(db: &Connection) -> rusqlite::Result<Vec<String>> {
    db.prepare_cached("SELECT repository FROM paused ORDER BY repository")?
        .query_map([], |row| row.get(0))?
        .collect()
}

pub(super) fn host_samples(db: &Connection, since: u64) -> rusqlite::Result<Vec<api::HostSample>> {
    db.prepare_cached("SELECT at, cpu, memory FROM host_samples WHERE at >= ?1 ORDER BY at")?
        .query_map([since.saturating_mul(1000)], |row| {
            Ok(api::HostSample {
                at: secs(row.get(0)?),
                cpu: row.get(1)?,
                memory: row.get(2)?,
            })
        })?
        .collect()
}

pub(super) fn stats(
    db: &Connection,
    query: &api::StatsQuery,
    now: u64,
) -> rusqlite::Result<api::Stats> {
    let bucket = match query.bucket {
        api::Bucket::Hour => 3600 * 1000,
        api::Bucket::Day => DAY_MS,
    };
    let end = query.until.unwrap_or(now).min(now).saturating_mul(1000);
    let since = query
        .since
        .map_or_else(
            || now.saturating_mul(1000).saturating_sub(30 * DAY_MS),
            |since| since.saturating_mul(1000),
        )
        .max(end.saturating_sub(STATS_BUCKETS * bucket));
    let until = query.until.map_or(i64::MAX, |until| {
        i64::try_from(until.saturating_mul(1000)).unwrap_or(i64::MAX)
    });
    let rows = db
        .prepare_cached(
            "SELECT j.queued_at / ?1 * ?1, j.repository, count(*), \
             count(*) FILTER (WHERE j.conclusion = 'success'), \
             count(*) FILTER (WHERE j.state = 'lost' OR j.conclusion IN ('failure', 'timed_out')), \
             count(*) FILTER (WHERE j.conclusion = 'cancelled'), \
             sum(max(coalesce(j.started_at, j.queued_at) - j.queued_at, 0)) / 1000, \
             sum(max(coalesce(j.finished_at, j.started_at, 0) - coalesce(j.started_at, 0), 0)) / 1000, \
             sum(coalesce(j.cpu_time, 0)), max(coalesce(j.memory_peak, 0)), \
             sum(coalesce(j.network_received, 0)), sum(coalesce(r.mirror_bytes, 0)) \
             FROM jobs j LEFT JOIN runners r ON r.name = j.runner \
             WHERE j.state IN ('completed', 'lost') AND j.queued_at >= ?2 AND j.queued_at < ?3 \
             AND (?4 IS NULL OR j.repository = ?4) \
             GROUP BY 1, 2 ORDER BY 1, 2",
        )?
        .query_map(
            params![bucket, since, until, query.repository],
            |row| {
                Ok(api::StatsRow {
                    start: secs(row.get(0)?),
                    repository: row.get(1)?,
                    jobs: row.get(2)?,
                    succeeded: row.get(3)?,
                    failed: row.get(4)?,
                    cancelled: row.get(5)?,
                    queue_seconds: row.get(6)?,
                    run_seconds: row.get(7)?,
                    cpu_time: row.get(8)?,
                    memory_peak: row.get(9)?,
                    network_received: row.get(10)?,
                    mirror_bytes: row.get(11)?,
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(api::Stats {
        bucket: query.bucket,
        rows,
    })
}

/// The finished jobs queued at or after `since` (ms) with their peaks, and the runners that held a slot after it.
pub(super) fn sizing(db: &Connection, since: u64) -> rusqlite::Result<sizing::History> {
    let ran = |row: &Row<'_>| {
        Ok(sizing::Ran {
            repository: row.get(0)?,
            workflow: row.get(1)?,
            name: row.get(2)?,
            profile: row.get(3)?,
            memory_cap: row.get(4)?,
            memory_peak: row.get(5)?,
            cpu_peak: row.get(6)?,
            queued_at: row.get(7)?,
            started_at: row.get(8)?,
            finished_at: row.get(9)?,
            oom_killed: row.get(10)?,
            reserved: row.get(11)?,
        })
    };
    let jobs = db
        .prepare_cached(
            "SELECT j.repository, j.workflow, j.name, r.profile, r.memory_cap, j.memory_peak, \
             coalesce(j.cpu_peak, 0), j.queued_at, j.started_at, j.finished_at, \
             coalesce(r.oom_kills, 0) > 0, json_extract(r.admission, '$.reserved') \
             FROM jobs j JOIN runners r ON r.name = j.runner \
             WHERE j.state IN ('completed', 'lost') AND j.conclusion IS NOT 'cancelled' \
             AND j.memory_peak IS NOT NULL AND j.queued_at >= ?1 ORDER BY j.id",
        )?
        .query_map([since], ran)?
        .collect::<rusqlite::Result<_>>()?;
    let cancelled = db
        .prepare_cached(
            "SELECT j.repository, j.workflow, j.name, r.profile, r.memory_cap, coalesce(j.memory_peak, 0), \
             coalesce(j.cpu_peak, 0), j.queued_at, j.started_at, j.finished_at, \
             coalesce(r.oom_kills, 0) > 0, json_extract(r.admission, '$.reserved') \
             FROM jobs j JOIN runners r ON r.name = j.runner \
             WHERE j.state = 'completed' AND j.conclusion = 'cancelled' AND j.queued_at >= ?1 ORDER BY j.id",
        )?
        .query_map([since], ran)?
        .collect::<rusqlite::Result<_>>()?;
    let runners = db
        .prepare_cached(
            "SELECT profile, started_at, removed_at FROM runners \
             WHERE removed_at IS NULL OR removed_at >= ?1",
        )?
        .query_map([since], |row| {
            Ok(sizing::Held {
                profile: row.get(0)?,
                started_at: row.get(1)?,
                removed_at: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(sizing::History {
        jobs,
        cancelled,
        runners,
    })
}

pub(super) fn latest_cache_sizes(db: &Connection) -> rusqlite::Result<super::Measured> {
    let at: Option<u64> = db.query_row("SELECT max(at) FROM cache_sizes", [], |row| row.get(0))?;
    let Some(at) = at else {
        return Ok(super::Measured::default());
    };
    let bytes = db
        .prepare_cached("SELECT name, bytes FROM cache_sizes WHERE at = ?1 ORDER BY name")?
        .query_map([at], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(super::Measured {
        at: Some(secs(at)),
        bytes,
    })
}

pub(super) fn cache_history(db: &Connection, since: u64) -> rusqlite::Result<Vec<api::CacheSize>> {
    db.prepare_cached("SELECT at, name, bytes FROM cache_sizes WHERE at >= ?1 ORDER BY at, name")?
        .query_map([since.saturating_mul(1000)], |row| {
            Ok(api::CacheSize {
                at: secs(row.get(0)?),
                name: row.get(1)?,
                bytes: row.get(2)?,
            })
        })?
        .collect()
}

pub(super) fn open_jobs(db: &Connection) -> rusqlite::Result<Vec<OpenJob>> {
    db.prepare_cached(
        "SELECT j.id, j.repository, j.state, j.runner, r.removed_at FROM jobs j \
         LEFT JOIN runners r ON r.name = j.runner WHERE j.state IN ('queued', 'running') ORDER BY j.id",
    )?
    .query_map([], |row| {
        Ok(OpenJob {
            id: row.get(0)?,
            repository: row.get(1)?,
            state: state(&row.get::<_, String>(2)?),
            runner: row.get(3)?,
            runner_removed_at: row.get(4)?,
        })
    })?
    .collect()
}

pub(super) fn runner_job(db: &Connection, runner: &str) -> rusqlite::Result<Option<u64>> {
    db.query_row(
        "SELECT id FROM jobs WHERE runner = ?1 ORDER BY id DESC LIMIT 1",
        [runner],
        |row| row.get(0),
    )
    .optional()
}

pub(super) fn log_position(db: &Connection, job: u64) -> rusqlite::Result<LogPosition> {
    Ok(db
        .query_row(
            "SELECT log_page, log_offset FROM jobs WHERE id = ?1",
            [job],
            |row| {
                Ok(LogPosition {
                    page: row.get(0)?,
                    offset: row.get(1)?,
                })
            },
        )
        .optional()?
        .unwrap_or(LogPosition { page: 1, offset: 0 }))
}

pub(super) fn mirror_log_until(db: &Connection) -> rusqlite::Result<super::MirrorPosition> {
    let value = |key: &str| -> rusqlite::Result<u64> {
        Ok(db
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?
            .unwrap_or(0))
    };
    Ok(super::MirrorPosition {
        at: value("mirror_log_until")?,
        seen: value("mirror_log_seen")?,
    })
}
