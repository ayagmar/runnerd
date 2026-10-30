//! Queries: job records with their timelines and metrics, samples, logs, events, stats, cache sizes and paused
//! repositories, turned into `api` types. Milliseconds in the database become seconds in the API. Shared by the
//! API's reads and by the writer, which reads back what it announces.

use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use tracing::warn;

use super::{LogPosition, OpenJob, SlotJob};
use crate::admission::Key;
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
     r.memory_cap, r.cpu_cap, coalesce(r.build_cache_hits, 0), coalesce(r.build_cache_misses, 0), \
     coalesce(r.build_cache_read_bytes, 0), coalesce(r.build_cache_written_bytes, 0), \
     coalesce(r.build_cache_refused, 0), coalesce(r.build_cache_writer, 0), r.sock_throttled, j.unseen, \
     j.cpu_periods, j.cpu_throttled, j.cpu_stalled FROM jobs j LEFT JOIN runners r ON r.name = j.runner";

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
            unseen: secs(row.get(39)?),
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
            build_cache: api::BuildCacheCounts {
                hits: row.get(32)?,
                misses: row.get(33)?,
                read_bytes: row.get(34)?,
                written_bytes: row.get(35)?,
                refused: row.get(36)?,
                writer: row.get(37)?,
            },
            sock_throttled: row.get(38)?,
            cpu_periods: row.get(40)?,
            cpu_throttled: row.get(41)?,
            cpu_stalled: row.get(42)?,
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
        let (mirror_bytes, build_cache, sock_throttled) = (
            summary.metrics.mirror_bytes,
            summary.metrics.build_cache,
            summary.metrics.sock_throttled,
        );
        summary.metrics = metrics(db, id)?;
        summary.metrics.mirror_bytes = mirror_bytes;
        summary.metrics.build_cache = build_cache;
        summary.metrics.sock_throttled = sock_throttled;
    }
    Ok(Some(summary))
}

/// A job's use over its samples: from its start to its end when known, else over its container's whole life.
/// `mirror_bytes`, `build_cache` and `sock_throttled` are left empty.
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
            "SELECT at, cpu_usage, network_received, network_sent, cpu_periods, cpu_throttled, cpu_stalled \
             FROM samples WHERE runner = ?1 AND at < ?2 ORDER BY at DESC LIMIT 1",
            params![runner, from],
            |row| {
                Ok(Baseline {
                    since: row.get(0)?,
                    usage: row.get(1)?,
                    received: row.get(2)?,
                    sent: row.get(3)?,
                    periods: row.get(4)?,
                    throttled: row.get(5)?,
                    stalled: row.get(6)?,
                })
            },
        )
        .optional()?;
    let baseline = if let Some(before) = before {
        before
    } else {
        let started: Option<u64> = db
            .query_row(
                "SELECT started_at FROM runners WHERE name = ?1",
                [runner],
                |row| row.get(0),
            )
            .optional()?;
        Baseline {
            since: started.unwrap_or(from),
            usage: 0,
            received: 0,
            sent: 0,
            periods: Some(0),
            throttled: Some(0),
            stalled: Some(0),
        }
    };
    let to = i64::try_from(to).unwrap_or(i64::MAX);
    db.query_row(
        "SELECT max(memory), CAST(avg(memory) AS INTEGER), max(cpu), max(cpu_usage), max(at), \
         max(network_received), max(network_sent), count(*), max(cpu_periods), count(cpu_periods), \
         max(cpu_throttled), count(cpu_throttled), max(cpu_stalled), count(cpu_stalled) \
         FROM samples WHERE runner = ?1 AND at >= ?2 AND at <= ?3",
        params![runner, from, to],
        |row| {
            let usage = row
                .get::<_, Option<u64>>(3)?
                .map(|usage| usage.saturating_sub(baseline.usage));
            let wall = row
                .get::<_, Option<u64>>(4)?
                .map(|last| last.saturating_sub(baseline.since));
            let samples: u64 = row.get(7)?;
            // A counter only every sample of the window and its baseline hold: one a sample lacks, as one taken before
            // runnerd read it, may hold what the container used before the job.
            let counted = |last: usize, before: Option<u64>| -> rusqlite::Result<Option<u64>> {
                let found: u64 = row.get(last + 1)?;
                Ok(row
                    .get::<_, Option<u64>>(last)?
                    .zip(before)
                    .filter(|_| found == samples)
                    .map(|(last, before)| last.saturating_sub(before)))
            };
            Ok(api::JobMetrics {
                memory_peak: row.get(0)?,
                memory_average: row.get(1)?,
                cpu_peak: row.get(2)?,
                // Microseconds to milliseconds.
                cpu_time: usage.map(|usage| usage / 1000),
                // CPU microseconds per wall millisecond are millicores.
                cpu_average: usage
                    .zip(wall)
                    .map(|(usage, wall)| usage.checked_div(wall).unwrap_or(0)),
                network_received: row
                    .get::<_, Option<u64>>(5)?
                    .map(|received| received.saturating_sub(baseline.received)),
                network_sent: row
                    .get::<_, Option<u64>>(6)?
                    .map(|sent| sent.saturating_sub(baseline.sent)),
                mirror_bytes: 0,
                build_cache: api::BuildCacheCounts::default(),
                sock_throttled: None,
                cpu_periods: counted(8, baseline.periods)?,
                cpu_throttled: counted(10, baseline.throttled)?,
                cpu_stalled: counted(12, baseline.stalled)?.map(|stalled| stalled / 1000),
            })
        },
    )
}

/// A runner's counters before a job's window: its last sample before it, or zero at its container's start.
struct Baseline {
    since: u64,
    usage: u64,
    received: u64,
    sent: u64,
    periods: Option<u64>,
    throttled: Option<u64>,
    stalled: Option<u64>,
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
    if let Some(workflow) = &query.workflow {
        condition("j.workflow =", Value::Text(workflow.clone()));
    }
    if let Some(name) = &query.name {
        condition("j.name =", Value::Text(name.clone()));
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

pub(super) fn sizing_change(row: &Row<'_>) -> rusqlite::Result<api::SizingChange> {
    Ok(api::SizingChange {
        profile: row.get("profile")?,
        at: secs(row.get("at")?),
        from: api::Resize {
            memory: row.get("from_memory")?,
            slots: row.get("from_slots")?,
            cpus: row.get("from_cpus")?,
        },
        to: api::Resize {
            memory: row.get("to_memory")?,
            slots: row.get("to_slots")?,
            cpus: row.get("to_cpus")?,
        },
        reason: row.get("reason")?,
        uncertain: row.get::<_, String>("state")? == "uncertain",
    })
}

/// The changes auto sizing wrote, or may have, since `since` (ms), oldest first.
pub(super) fn sizing_changes(
    db: &Connection,
    since: u64,
) -> rusqlite::Result<Vec<api::SizingChange>> {
    db.prepare_cached(
        "SELECT * FROM auto_sizing WHERE state IN ('done', 'uncertain') AND at >= ?1 ORDER BY at, id",
    )?
    .query_map([since], sizing_change)?
    .collect()
}

/// The latest pending, done or uncertain change of each profile, and the pending ones.
pub(super) fn auto_sizing(db: &Connection) -> rusqlite::Result<super::AutoSizing> {
    let last = db
        .prepare_cached(
            "SELECT profile, MAX(at) FROM auto_sizing WHERE state IN ('pending', 'done', 'uncertain') GROUP BY profile",
        )?
        .query_map([], |row| Ok((row.get(0)?, secs(row.get(1)?))))?
        .collect::<rusqlite::Result<_>>()?;
    let pending = db
        .prepare_cached("SELECT * FROM auto_sizing WHERE state = 'pending' ORDER BY id")?
        .query_map([], |row| Ok((row.get("id")?, sizing_change(row)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let cpus = db
        .prepare_cached(
            "SELECT json_extract(body, '$.profile.name'), json_extract(body, '$.profile.cpus'), at, \
             json_extract(body, '$.profile.memory') FROM events \
             WHERE json_extract(body, '$.type') = 'profile.changed' ORDER BY id",
        )?
        .query_map([], |row| {
            Ok((
                sizing::CpusSet {
                    profile: row.get(0)?,
                    cpus: row.get(1)?,
                    at: row.get(2)?,
                },
                row.get::<_, Option<u64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let memory = cpus
        .iter()
        .filter_map(|(set, memory): &(sizing::CpusSet, Option<u64>)| {
            memory.map(|memory| sizing::MemorySet {
                profile: set.profile.clone(),
                memory,
                at: set.at,
            })
        })
        .collect();
    Ok(super::AutoSizing {
        last,
        pending,
        cpus: cpus.into_iter().map(|(set, _)| set).collect(),
        memory,
    })
}

/// The newest durable event's ID; 0 before the first.
pub(super) fn last_event(db: &Connection) -> rusqlite::Result<u64> {
    db.query_row("SELECT coalesce(max(id), 0) FROM events", [], |row| {
        row.get(0)
    })
}

/// What the notifier keeps for each target, by target, then key.
pub(super) fn notify_state(
    db: &Connection,
) -> rusqlite::Result<std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>>> {
    let mut targets = std::collections::BTreeMap::<_, std::collections::BTreeMap<_, _>>::new();
    let rows = db
        .prepare_cached("SELECT target, key, value FROM notify ORDER BY target, key")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<(String, String, u64)>>>()?;
    for (target, key, value) in rows {
        targets.entry(target).or_default().insert(key, value);
    }
    Ok(targets)
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

/// From when the history holds every final job, in ms.
fn kept_since(db: &Connection) -> rusqlite::Result<u64> {
    db.query_row("SELECT jobs_since FROM history_kept", [], |row| row.get(0))
}

/// [`sizing::Wait::counted`] in SQL, in ms, for the job `j` with the history's `jobs_since` as `?1`; 0 for a job that
/// did not start.
const COUNTED_WAIT: &str =
    "max(coalesce(j.started_at, j.queued_at) - max(j.queued_at, ?1) - j.unseen, 0)";

/// What `Store::activity` reads. Jobs another runner ran are left out: their wait is not for runnerd.
pub(super) fn activity(db: &Connection, since: u64, now: u64) -> rusqlite::Result<api::Activity> {
    let ms = |secs: u64| i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let kept_since = kept_since(db)?;
    let started = db
        .prepare_cached(
            "SELECT queued_at, started_at, unseen FROM jobs \
             WHERE started_at >= ?1 AND started_at <= ?2 AND state != 'elsewhere' ORDER BY started_at",
        )?
        .query_map(params![ms(since), ms(now)], |row| {
            let wait = sizing::Wait {
                queued_at: row.get(0)?,
                started_at: row.get(1)?,
                unseen: row.get(2)?,
            };
            Ok(api::Started {
                at: secs(wait.started_at),
                waited: secs(wait.counted(kept_since)),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (finished, failed, runner_failed) = db.query_row(
        "SELECT count(*), count(*) FILTER (WHERE j.state = 'lost' OR j.conclusion IN ('failure', 'timed_out')), \
         count(*) FILTER (WHERE j.state = 'lost' OR (j.conclusion IN ('failure', 'timed_out') AND r.oom_kills > 0)) \
         FROM jobs j LEFT JOIN runners r ON r.name = j.runner \
         WHERE j.finished_at >= ?1 AND j.finished_at <= ?2 AND j.state IN ('completed', 'lost')",
        params![ms(since), ms(now)],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(api::Activity {
        since,
        until: now,
        started,
        finished,
        failed,
        runner_failed,
    })
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
        .prepare_cached(&format!(
            "SELECT j.queued_at / ?2 * ?2, j.repository, count(*), \
             count(*) FILTER (WHERE j.conclusion = 'success'), \
             count(*) FILTER (WHERE j.state = 'lost' OR j.conclusion IN ('failure', 'timed_out')), \
             count(*) FILTER (WHERE j.conclusion = 'cancelled'), \
             sum({COUNTED_WAIT}) / 1000, \
             sum(max(coalesce(j.finished_at, j.started_at, 0) - coalesce(j.started_at, 0), 0)) / 1000, \
             sum(coalesce(j.cpu_time, 0)), max(coalesce(j.memory_peak, 0)), \
             sum(coalesce(j.network_received, 0)), sum(coalesce(r.mirror_bytes, 0)) \
             FROM jobs j LEFT JOIN runners r ON r.name = j.runner \
             WHERE j.state IN ('completed', 'lost') AND j.queued_at >= ?3 AND j.queued_at < ?4 \
             AND (?5 IS NULL OR j.repository = ?5) \
             GROUP BY 1, 2 ORDER BY 1, 2",
        ))?
        .query_map(
            params![kept_since(db)?, bucket, since, until, query.repository],
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
            succeeded: row.get(12)?,
            hit_cap: row.get::<_, Option<u64>>(13)?.map(|ooms| ooms > 0),
            measure: if row.get(14)? {
                sizing::Measure::WithoutPageCache
            } else {
                sizing::Measure::WithActiveFile
            },
            cpu_average: row.get(15)?,
            cpu_cap: row.get(16)?,
            runner_started_at: row.get(17)?,
            host_waited: row.get(18)?,
            own_cap: row.get::<_, Option<u64>>(19)?.map(|ooms| ooms > 0),
            run_id: row.get(20)?,
            branch: row.get(21)?,
            failed: row.get(22)?,
            unseen: row.get(23)?,
            cpu_periods: row.get(24)?,
            cpu_throttled: row.get(25)?,
        })
    };
    let jobs = db
        .prepare_cached(
            "SELECT j.repository, j.workflow, j.name, r.profile, r.memory_cap, j.memory_peak, \
             coalesce(j.cpu_peak, 0), j.queued_at, j.started_at, j.finished_at, \
             coalesce(r.oom_kills, 0) > 0, json_extract(r.admission, '$.reserved'), \
             j.state = 'completed' AND j.conclusion IS 'success', r.ooms, r.memory_measure IS 'without_page_cache', \
             j.cpu_average, r.cpu_cap, r.started_at, j.host_waited, r.local_ooms, j.run_id, j.branch, \
             j.state = 'completed' AND j.conclusion IN ('failure', 'timed_out'), j.unseen, j.cpu_periods, \
             j.cpu_throttled FROM jobs j \
             JOIN runners r ON r.name = j.runner \
             WHERE j.state IN ('completed', 'lost') AND j.conclusion IS NOT 'cancelled' \
             AND j.memory_peak IS NOT NULL AND j.queued_at >= ?1 ORDER BY j.id",
        )?
        .query_map([since], ran)?
        .collect::<rusqlite::Result<_>>()?;
    let cancelled = db
        .prepare_cached(
            "SELECT j.repository, j.workflow, j.name, r.profile, r.memory_cap, coalesce(j.memory_peak, 0), \
             coalesce(j.cpu_peak, 0), j.queued_at, j.started_at, j.finished_at, \
             coalesce(r.oom_kills, 0) > 0, json_extract(r.admission, '$.reserved'), 0, r.ooms, \
             r.memory_measure IS 'without_page_cache', j.cpu_average, r.cpu_cap, r.started_at, j.host_waited, \
             r.local_ooms, j.run_id, j.branch, 0, j.unseen, j.cpu_periods, j.cpu_throttled FROM jobs j \
             JOIN runners r ON r.name = j.runner \
             WHERE j.state = 'completed' AND j.conclusion = 'cancelled' AND j.queued_at >= ?1 ORDER BY j.id",
        )?
        .query_map([since], ran)?
        .collect::<rusqlite::Result<_>>()?;
    let runners = db
        .prepare_cached(
            // From its job's queueing when it came later, as for a warm runner; a runner that took no job held
            // no slot for one.
            "SELECT r.profile, max(r.started_at, j.queued_at), r.removed_at FROM runners r \
             JOIN jobs j ON j.runner = r.name WHERE r.removed_at IS NULL OR r.removed_at >= ?1",
        )?
        .query_map([since], |row| {
            Ok(sizing::Held {
                profile: row.get(0)?,
                started_at: row.get(1)?,
                removed_at: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let kept_since = kept_since(db)?;
    Ok(sizing::History {
        jobs,
        cancelled,
        runners,
        kept_since,
        ..sizing::History::default()
    })
}

/// The completed and lost jobs dated at or after `since` (ms) by their end, start or queue time, as
/// `api::PricedRun::at` dates them; each repository's visibility; and since when the history holds every final job.
pub(super) fn pricing(db: &Connection, since: u64) -> rusqlite::Result<api::PricingHistory> {
    let runs = db
        .prepare_cached(
            "SELECT j.repository, j.workflow, j.labels, j.state, r.profile, j.queued_at, j.started_at, \
             j.finished_at FROM jobs j LEFT JOIN runners r ON r.name = j.runner \
             WHERE j.state IN ('completed', 'lost') AND coalesce(j.finished_at, j.started_at, j.queued_at) >= ?1 \
             ORDER BY j.id",
        )?
        .query_map([since], |row| {
            let labels: String = row.get(2)?;
            Ok(api::PricedRun {
                repository: row.get(0)?,
                workflow: row.get(1)?,
                labels: serde_json::from_str(&labels).unwrap_or_default(),
                state: state(&row.get::<_, String>(3)?),
                profile: row.get(4)?,
                queued_at: row.get(5)?,
                started_at: row.get(6)?,
                finished_at: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let visibility = db
        .prepare_cached("SELECT name, visibility FROM repositories")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|row| match row {
            Ok((name, text)) => match text.as_str() {
                "public" => Some(Ok((name, api::Visibility::Public))),
                "private" => Some(Ok((name, api::Visibility::Private))),
                "internal" => Some(Ok((name, api::Visibility::Internal))),
                _ => None,
            },
            Err(error) => Some(Err(error)),
        })
        .collect::<rusqlite::Result<_>>()?;
    let kept_since = kept_since(db)?;
    Ok(api::PricingHistory {
        runs,
        visibility,
        kept_since,
    })
}

/// The jobs completed or lost that were queued at or after `since` (ms), with their runners, as insights read them,
/// and from when the history holds every job.
pub(super) fn insights(db: &Connection, since: u64) -> rusqlite::Result<crate::insights::History> {
    let runs = db.prepare_cached(
        "SELECT j.id, j.repository, j.workflow, j.name, j.branch, j.url, j.run_id, j.run_attempt, j.state, \
         j.conclusion, j.queued_at, j.started_at, j.finished_at, r.started_at, r.profile, j.memory_peak, \
         r.build_cache_hits, r.build_cache_misses, r.build_cache_read_bytes, r.build_cache_written_bytes, \
         r.build_cache_refused, r.build_cache_writer, r.sock_throttled, j.unseen, r.cpu_cap, j.cpu_periods, \
         j.cpu_throttled FROM jobs j LEFT JOIN runners r ON r.name = j.runner \
         WHERE j.state IN ('completed', 'lost') AND j.queued_at >= ?1 ORDER BY j.id",
    )?
    .query_map([since], |row| {
        Ok(crate::insights::Run {
            id: row.get(0)?,
            repository: row.get(1)?,
            workflow: row.get(2)?,
            name: row.get(3)?,
            branch: row.get(4)?,
            url: row.get(5)?,
            run_id: row.get(6)?,
            run_attempt: row.get(7)?,
            state: state(&row.get::<_, String>(8)?),
            conclusion: row.get(9)?,
            queued_at: row.get(10)?,
            started_at: row.get(11)?,
            finished_at: row.get(12)?,
            runner_started_at: row.get(13)?,
            profile: row.get(14)?,
            memory_peak: row.get(15)?,
            build_cache: match row.get::<_, Option<bool>>(21)? {
                Some(writer) => Some(api::BuildCacheCounts {
                    hits: row.get(16)?,
                    misses: row.get(17)?,
                    read_bytes: row.get(18)?,
                    written_bytes: row.get(19)?,
                    refused: row.get(20)?,
                    writer,
                }),
                None => None,
            },
            sock_throttled: row.get(22)?,
            unseen: row.get(23)?,
            cpu_cap: row.get(24)?,
            cpu_periods: row.get(25)?,
            cpu_throttled: row.get(26)?,
        })
    })?
    .collect::<rusqlite::Result<_>>()?;
    let kept_since = kept_since(db)?;
    Ok(crate::insights::History { runs, kept_since })
}

pub(super) fn warm(
    db: &Connection,
    since: u64,
) -> rusqlite::Result<Vec<crate::scheduler::warm::Seen>> {
    db.prepare_cached(
        "SELECT repository, workflow, run_id, run_attempt, queued_at, labels FROM jobs WHERE queued_at >= ?1 \
         ORDER BY id",
    )?
    .query_map([since], |row| {
        let labels: String = row.get(5)?;
        Ok(crate::scheduler::warm::Seen {
            repository: row.get(0)?,
            labels: serde_json::from_str(&labels).unwrap_or_default(),
            workflow: row.get(1)?,
            run_id: row.get(2)?,
            run_attempt: row.get(3)?,
            queued_at: row.get(4)?,
        })
    })?
    .collect()
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

/// What the build cache served and took from the runners started since `since` (ms), summed.
pub(super) fn build_cache_since(
    db: &Connection,
    since: u64,
) -> rusqlite::Result<api::BuildCacheCounts> {
    db.query_row(
        "SELECT coalesce(sum(build_cache_hits), 0), coalesce(sum(build_cache_misses), 0), \
         coalesce(sum(build_cache_read_bytes), 0), coalesce(sum(build_cache_written_bytes), 0), \
         coalesce(sum(build_cache_refused), 0) FROM runners WHERE started_at >= ?1 AND build_cache_writer IS NOT NULL",
        [since],
        |row| {
            Ok(api::BuildCacheCounts {
                hits: row.get(0)?,
                misses: row.get(1)?,
                read_bytes: row.get(2)?,
                written_bytes: row.get(3)?,
                refused: row.get(4)?,
                writer: false,
            })
        },
    )
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
        "SELECT j.id, j.repository, j.state, j.runner, r.removed_at, j.run_id, j.workflow, j.queued_at, j.run_attempt, \
         j.labels FROM jobs j \
         LEFT JOIN runners r ON r.name = j.runner WHERE j.state IN ('queued', 'running') ORDER BY j.id",
    )?
    .query_map([], |row| {
        Ok(OpenJob {
            id: row.get(0)?,
            repository: row.get(1)?,
            state: state(&row.get::<_, String>(2)?),
            runner: row.get(3)?,
            runner_removed_at: row.get(4)?,
            run_id: row.get(5)?,
            workflow: row.get(6)?,
            queued_at: row.get(7)?,
            run_attempt: row.get(8)?,
            labels: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
        })
    })?
    .collect()
}

pub(super) fn slot_jobs(db: &Connection) -> rusqlite::Result<Vec<SlotJob>> {
    // The bare columns of a `max()` aggregate come from the row that has the maximum.
    let newest: Vec<(SlotJob, u64)> = db
        .prepare_cached(
            "SELECT r.profile, r.slot, j.repository, j.workflow, j.name, \
             coalesce(j.finished_at, j.started_at, j.queued_at), max(j.id) FROM jobs j \
             JOIN runners r ON r.name = j.runner GROUP BY r.profile, r.slot",
        )?
        .query_map([], |row| {
            Ok((
                SlotJob {
                    profile: row.get(0)?,
                    slot: row.get(1)?,
                    key: Key {
                        repository: row.get(2)?,
                        workflow: row.get(3)?,
                        name: row.get(4)?,
                    },
                },
                row.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    // A reseed that made a snapshot dropped every slot's layer of its profile: what ran before it left nothing.
    let mut reseeded = std::collections::BTreeMap::<String, u64>::new();
    let bodies: Vec<(u64, String)> = db
        .prepare_cached("SELECT at, body FROM events WHERE body LIKE '%\"cache.reseeded\"%'")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (at, body) in bodies {
        if let Ok(api::Event::CacheReseeded {
            profile,
            output,
            error,
        }) = serde_json::from_str(&body)
            && (error.is_some() || crate::cache::seeded(&output))
        {
            let latest = reseeded.entry(profile).or_default();
            *latest = (*latest).max(at);
        }
    }
    // A job no listing named leaves its slot unknown.
    Ok(newest
        .into_iter()
        .filter(|(job, at)| {
            !job.key.name.is_empty() && reseeded.get(&job.profile).is_none_or(|made| at > made)
        })
        .map(|(job, _)| job)
        .collect())
}

pub(super) fn runner_job(db: &Connection, runner: &str) -> rusqlite::Result<Option<u64>> {
    db.query_row(
        "SELECT id FROM jobs WHERE runner = ?1 ORDER BY id DESC LIMIT 1",
        [runner],
        |row| row.get(0),
    )
    .optional()
}

pub(super) fn runner_admission(
    db: &Connection,
    runner: &str,
) -> rusqlite::Result<Option<api::Admission>> {
    let admission: Option<String> = db
        .query_row(
            "SELECT admission FROM runners WHERE name = ?1",
            [runner],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(admission.and_then(|admission| serde_json::from_str(&admission).ok()))
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
