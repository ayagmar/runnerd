//! Queries: job records with their timelines and metrics, samples, logs, events, stats, cache sizes and paused
//! repositories, turned into `api` types. Milliseconds in the database become seconds in the API. Shared by the
//! API's reads and by the writer, which reads back what it announces.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use tracing::warn;

use super::{LogPosition, OpenJob, SlotJob};
use crate::admission::Key;
use crate::admission::trace::{Outside, Point, RunId, RunTrace, Trace};
use crate::api::{self, EventEnvelope, JobState};
use crate::sizing;

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

pub(super) fn state(name: &str) -> JobState {
    match name {
        "running" => JobState::Running,
        "completed" => JobState::Completed,
        "lost" => JobState::Lost,
        "elsewhere" => JobState::Elsewhere,
        _ => JobState::Queued,
    }
}

/// [`api::RecordMark`] of the job `j` in SQL, from its `records` row `rec`, in `RecordNote::state`'s precedence.
/// A macro, so that `SUMMARY` and `RECORD_MARK` read one expression.
macro_rules! record_mark {
    () => {
        "CASE WHEN rec.job IS NULL THEN NULL WHEN rec.skipped IS NOT NULL THEN 'skipped' \
         WHEN rec.removed_at IS NOT NULL OR rec.gone_at IS NOT NULL THEN 'removed' ELSE 'kept' END"
    };
}

/// The columns `summary_from` reads, in its order.
const SUMMARY: &str = concat!(
    "j.id, j.repository, j.run_id, j.run_attempt, j.workflow, j.name, j.branch, j.url, j.labels, \
     j.state, j.conclusion, j.runner, r.profile, r.slot, j.queued_at, r.started_at, j.started_at, j.finished_at, \
     r.removed_at, j.cpu_time, j.cpu_peak, j.cpu_average, j.memory_peak, j.memory_average, j.network_received, \
     j.network_sent, coalesce(r.mirror_bytes, 0), j.log_lines, j.log_bytes, j.log_truncated, \
     r.memory_cap, r.cpu_cap, coalesce(r.build_cache_hits, 0), coalesce(r.build_cache_misses, 0), \
     coalesce(r.build_cache_read_bytes, 0), coalesce(r.build_cache_written_bytes, 0), \
     coalesce(r.build_cache_refused, 0), coalesce(r.build_cache_writer, 0), r.sock_throttled, j.unseen, \
     j.cpu_periods, j.cpu_throttled, j.cpu_stalled, j.seen_at, j.runner_fault, ",
    record_mark!(),
    " FROM jobs j LEFT JOIN runners r ON r.name = j.runner LEFT JOIN records rec ON rec.job = j.id"
);

/// [`api::RecordMark`] of the job `j` in SQL: the expression `SUMMARY` reads.
const RECORD_MARK: &str = record_mark!();

/// The text columns of `SUMMARY` a list cuts ([`listed_text`]).
const SUMMARY_TEXT: &str =
    "j.repository, j.run_id, j.run_attempt, j.workflow, j.name, j.branch, j.url, j.labels,";

/// `column` cut to what a list shows, in SQL: its first [`api::JOB_TEXT_MAX`] bytes, which [`text`] reads to its last
/// whole character.
fn listed(column: &str) -> String {
    byte_prefix(column, api::JOB_TEXT_MAX)
}

/// The first `max` bytes of the text `column`, in SQL, as a blob: `substr` of a text stops at its first NUL, of a
/// blob it does not. [`text`] reads it.
pub(super) fn byte_prefix(column: &str, max: usize) -> String {
    format!("substr(CAST({column} AS BLOB), 1, {max})")
}

/// A text column, or the prefix [`byte_prefix`] read of one, to its last whole character.
pub(super) fn text(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            let whole = match std::str::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default()
                }
            };
            Ok(Some(whole.to_owned()))
        }
        other => Err(rusqlite::Error::InvalidColumnType(
            index,
            String::new(),
            other.data_type(),
        )),
    }
}

/// The labels a list shows, in SQL: none past [`api::JOB_LABELS_MAX`] bytes.
fn listed_labels() -> String {
    format!(
        "CASE WHEN octet_length(j.labels) <= {} THEN j.labels ELSE '[]' END",
        api::JOB_LABELS_MAX
    )
}

/// `SUMMARY_TEXT` as a list reads it.
fn listed_text() -> String {
    format!(
        "{}, j.run_id, j.run_attempt, {}, {}, {}, {}, {},",
        listed("j.repository"),
        listed("j.workflow"),
        listed("j.name"),
        listed("j.branch"),
        listed("j.url"),
        listed_labels()
    )
}

/// Whether a list cuts any of the job `j`'s text, in SQL.
fn text_cut() -> String {
    let column = |column: &str| {
        format!(
            "coalesce(octet_length({column}) > {}, 0)",
            api::JOB_TEXT_MAX
        )
    };
    format!(
        "({} OR {} OR {} OR {} OR {} OR octet_length(j.labels) > {})",
        column("j.repository"),
        column("j.workflow"),
        column("j.name"),
        column("j.branch"),
        column("j.url"),
        api::JOB_LABELS_MAX
    )
}

/// The bytes of the job `j`'s text as a list shows it, which a page counts ([`api::row_bytes`]), in SQL.
fn row_bytes() -> String {
    format!(
        "octet_length({}) + coalesce(octet_length({}), 0) + octet_length({}) + coalesce(octet_length({}), 0) + \
         coalesce(octet_length({}), 0) + octet_length({})",
        listed("j.repository"),
        listed("j.workflow"),
        listed("j.name"),
        listed("j.branch"),
        listed("j.url"),
        listed_labels()
    )
}

/// How `RECORD_MARK` names a mark.
fn record_mark(name: &str) -> api::RecordMark {
    match name {
        "kept" => api::RecordMark::Kept,
        "removed" => api::RecordMark::Removed,
        "skipped" => api::RecordMark::Skipped,
        _ => api::RecordMark::Unknown,
    }
}

fn summary_from(row: &Row<'_>) -> rusqlite::Result<api::JobSummary> {
    let labels: String = row.get(8)?;
    let runner: Option<String> = row.get(11)?;
    let profile: Option<String> = row.get(12)?;
    let slot: Option<usize> = row.get(13)?;
    let seconds = |index: usize| row.get::<_, Option<u64>>(index).map(|ms| ms.map(secs));
    Ok(api::JobSummary {
        id: row.get(0)?,
        repository: text(row, 1)?.unwrap_or_default(),
        run_id: row.get(2)?,
        run_attempt: row.get(3)?,
        workflow: text(row, 4)?,
        name: text(row, 5)?.unwrap_or_default(),
        branch: text(row, 6)?,
        url: text(row, 7)?,
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
            seen_at: seconds(43)?,
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
        runner_fault: row.get(44)?,
        record_mark: row
            .get::<_, Option<String>>(45)?
            .as_deref()
            .map(record_mark),
    })
}

/// A job without its steps. Before a record is final, its metrics come from its samples so far.
pub(super) fn summary(db: &Connection, id: u64) -> rusqlite::Result<Option<api::JobSummary>> {
    let Some(summary) = db
        .query_row(
            &format!("SELECT {SUMMARY} WHERE j.id = ?1"),
            [id],
            summary_from,
        )
        .optional()?
    else {
        return Ok(None);
    };
    with_metrics(db, summary).map(Some)
}

/// A job as a list shows it ([`api::JOB_TEXT_MAX`]), and whether any of its text was cut.
fn listed_summary(db: &Connection, id: u64) -> rusqlite::Result<Option<(api::JobSummary, bool)>> {
    let select = SUMMARY.replacen(SUMMARY_TEXT, &listed_text(), 1).replacen(
        " FROM jobs j",
        &format!(", {} FROM jobs j", text_cut()),
        1,
    );
    let Some((summary, cut)) = db
        .query_row(&format!("SELECT {select} WHERE j.id = ?1"), [id], |row| {
            Ok((summary_from(row)?, row.get::<_, bool>(46)?))
        })
        .optional()?
    else {
        return Ok(None);
    };
    Ok(Some((with_metrics(db, summary)?, cut)))
}

/// `summary` with, before its record is final, its metrics from its samples so far.
fn with_metrics(
    db: &Connection,
    mut summary: api::JobSummary,
) -> rusqlite::Result<api::JobSummary> {
    let id = summary.id;
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
    Ok(summary)
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
        "SELECT created_at, bytes, skipped, removed_at, removed_by, gone_at FROM records WHERE job = ?1",
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
                gone_at: row.get::<_, Option<u64>>(5)?.map(secs),
            })
        },
    )
    .optional()
}

/// Runs `read` in one deferred read transaction: a snapshot of the database as of its first read, kept to its last,
/// so a response assembled from several reads is of one moment. Dropped, it rolls back: it wrote nothing.
pub(super) fn snapshot<T>(
    db: &Connection,
    read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    let snapshot = db.unchecked_transaction()?;
    read(&snapshot)
}

/// The states whose records are final, in SQL.
const FINAL: &str = "('completed', 'lost', 'elsewhere')";

/// [`api::JobSort::key`] of the job `j` in SQL, in seconds or bytes; NULL while unknown.
fn sort_key(sort: api::JobSort) -> String {
    match sort {
        api::JobSort::Queued => "j.queued_at / 1000".to_owned(),
        api::JobSort::Wait => "CASE WHEN j.started_at IS NOT NULL \
             THEN max(j.started_at / 1000 - j.queued_at / 1000, 0) END"
            .to_owned(),
        api::JobSort::Run => format!(
            "CASE WHEN j.state IN {FINAL} AND j.started_at IS NOT NULL AND j.finished_at IS NOT NULL \
             THEN max(j.finished_at / 1000 - j.started_at / 1000, 0) END"
        ),
        api::JobSort::Memory => format!("CASE WHEN j.state IN {FINAL} THEN j.memory_peak END"),
    }
}

/// A `WHERE` clause and its values, numbered as they are added.
#[derive(Default)]
struct Where {
    conditions: Vec<String>,
    values: Vec<Value>,
}

impl Where {
    /// Binds `value` and returns its placeholder, `?3`.
    fn value(&mut self, value: Value) -> String {
        self.values.push(value);
        format!("?{}", self.values.len())
    }

    /// Adds `condition`, each `?` in it the placeholder of `value`.
    fn add(&mut self, condition: &str, value: Value) {
        let placeholder = self.value(value);
        self.conditions.push(condition.replace('?', &placeholder));
    }
}

/// The `ORDER BY` of `query`'s page, with the condition that starts it after `before`.
fn order(query: &api::JobQuery, sql: &mut Where) -> String {
    let direction = match query.order.unwrap_or_default() {
        api::SortOrder::Desc => "DESC",
        api::SortOrder::Asc => "ASC",
    };
    if let Some(before) = query.before {
        sql.add(
            "j.id < ?",
            Value::Integer(i64::try_from(before).unwrap_or(i64::MAX)),
        );
    }
    match query.sort.unwrap_or(api::JobSort::Queued) {
        api::JobSort::Queued => format!("j.id {direction}"),
        sort => {
            let key = sort_key(sort);
            format!("({key}) IS NULL, ({key}) {direction}, j.id {direction}")
        }
    }
}

/// One page of jobs as `query` asks, which [`api::JobQuery::check`] passed: each page's IDs, sizes and how many the
/// filters admit read first, then each job within the page's rows and bytes, its text cut as a list shows it, all in
/// one snapshot.
pub(super) fn jobs(db: &Connection, query: &api::JobQuery) -> rusqlite::Result<api::JobList> {
    let mut sql = Where::default();
    let text = |text: &String| Value::Text(text.clone());
    if let Some(repository) = &query.repository {
        sql.add("j.repository = ?", text(repository));
    }
    if let Some(state) = query.state {
        sql.add("j.state = ?", Value::Text(state_name(state).to_owned()));
    }
    if let Some(conclusion) = &query.conclusion {
        sql.add("j.conclusion = ?", text(conclusion));
    }
    if let Some(workflow) = &query.workflow {
        sql.add("j.workflow = ?", text(workflow));
    }
    if let Some(name) = &query.name {
        sql.add("j.name = ?", text(name));
    }
    if let Some(branch) = &query.branch {
        sql.add("j.branch = ?", text(branch));
    }
    if let Some(q) = &query.q {
        // `instr` reads the whole text, NULs too, where `LIKE` stops at the first; `lower` folds ASCII only.
        sql.add(
            "(instr(lower(j.name), ?) > 0 OR instr(lower(j.workflow), ?) > 0 \
             OR instr(lower(j.branch), ?) > 0)",
            Value::Text(q.to_ascii_lowercase()),
        );
    }
    let integer = |value: u64| Value::Integer(i64::try_from(value).unwrap_or(i64::MAX));
    let ms = |secs: u64| integer(secs.saturating_mul(1000));
    if let Some(since) = query.since {
        sql.add("j.queued_at >= ?", ms(since));
    }
    if let Some(until) = query.until {
        sql.add("j.queued_at < ?", ms(until));
    }
    if let Some(record) = query.record {
        let marks = match record {
            api::RecordFilter::Any => "'kept', 'removed', 'skipped'",
            api::RecordFilter::Kept => "'kept'",
            api::RecordFilter::Removed => "'removed'",
            api::RecordFilter::Skipped => "'skipped'",
        };
        sql.conditions.push(format!("({RECORD_MARK}) IN ({marks})"));
    }
    let order = order(query, &mut sql);
    let filter = if sql.conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", sql.conditions.join(" AND "))
    };
    let values = sql.values;
    let size = query.page_size();
    let row_bytes = row_bytes();
    snapshot(db, |db| {
        let rows: Vec<(u64, u64, u64)> = db
            .prepare(&format!(
                "SELECT j.id, {row_bytes}, count(*) OVER () FROM jobs j \
                 LEFT JOIN records rec ON rec.job = j.id {filter} ORDER BY {order} LIMIT {}",
                size + 1
            ))?
            .query_map(params_from_iter(values), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut jobs = Vec::new();
        let mut shortened = Vec::new();
        let mut bytes = 0;
        for (id, row_bytes, _) in rows.iter().take(size as usize) {
            if !jobs.is_empty() && bytes + row_bytes > api::JOBS_PAGE_BYTES {
                break;
            }
            bytes += row_bytes;
            if let Some((job, cut)) = listed_summary(db, *id)? {
                if cut {
                    shortened.push(job.id);
                }
                jobs.push(job);
            }
        }
        let total = rows.first().map_or(0, |row| row.2);
        let more = rows.len() > jobs.len();
        Ok(query.answer(jobs, more, total, shortened))
    })
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

/// The stored lines of a job's log `read` asks for, their text read as UTF-8 with anything else replaced: what the
/// capture stores is text, but a history is a file anyone may have written. SQLite hands over only the prefix of
/// each line asked for, and the rows stop at the byte budget, so no read holds more than it asked for whatever the
/// lines' size.
pub(super) fn lines_from(
    db: &Connection,
    job: u64,
    read: api::LineRead,
) -> rusqlite::Result<Vec<api::StoredLine>> {
    let mut statement = db.prepare_cached(
        "SELECT line, substr(CAST(text AS BLOB), 1, ?3), octet_length(text) FROM log_lines \
         WHERE job = ?1 AND line >= ?2 ORDER BY line LIMIT ?4",
    )?;
    let mut rows = statement.query(params![job, read.from, read.prefix, read.limit])?;
    let mut lines = Vec::new();
    let mut bytes = 0;
    while bytes < read.bytes
        && let Some(row) = rows.next()?
    {
        let text = match row.get_ref(1)? {
            ValueRef::Text(text) | ValueRef::Blob(text) => text,
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => &[],
        };
        let line = api::StoredLine::cut(row.get(0)?, text, row.get(2)?);
        bytes += line.text.len() as u64;
        lines.push(line);
    }
    Ok(lines)
}

/// Every stored line of a job's log, in order.
pub(super) fn log_text(db: &Connection, job: u64) -> rusqlite::Result<Vec<String>> {
    db.prepare_cached("SELECT text FROM log_lines WHERE job = ?1 ORDER BY line")?
        .query_map([job], |row| row.get(0))?
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
    let events = db
        .prepare_cached(
            "SELECT json_extract(body, '$.profile.name'), json_extract(body, '$.profile.cpus'), at, \
             json_extract(body, '$.profile.memory'), json_array_length(body, '$.profile.slots') FROM events \
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
                row.get::<_, Option<u32>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let memory = events
        .iter()
        .filter_map(|(set, memory, _)| {
            memory.map(|memory| sizing::MemorySet {
                profile: set.profile.clone(),
                memory,
                at: set.at,
            })
        })
        .collect();
    let slots = events
        .iter()
        .filter_map(|(set, _, slots)| {
            slots.map(|slots| sizing::SlotsSet {
                profile: set.profile.clone(),
                slots,
                at: set.at,
            })
        })
        .collect();
    Ok(super::AutoSizing {
        last,
        pending,
        cpus: events.into_iter().map(|(set, _, _)| set).collect(),
        memory,
        slots,
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
) -> rusqlite::Result<BTreeMap<String, BTreeMap<String, u64>>> {
    let mut targets = BTreeMap::<_, BTreeMap<_, _>>::new();
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
    let mut profiles = BTreeMap::new();
    let mut repositories = BTreeMap::new();
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
pub(super) fn kept_since(db: &Connection) -> rusqlite::Result<u64> {
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
            slot_waited: row.get(26)?,
            run_attempt: row.get(27)?,
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
             j.cpu_throttled, j.slot_waited, j.run_attempt FROM jobs j \
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
             r.local_ooms, j.run_id, j.branch, 0, j.unseen, j.cpu_periods, j.cpu_throttled, j.slot_waited, \
             j.run_attempt FROM jobs j \
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
    let left_out = db
        .prepare_cached(
            "SELECT coalesce(sum(state = 'queued'), 0), coalesce(sum(state = 'running'), 0), \
             coalesce(sum(conclusion IS 'cancelled'), 0), coalesce(sum(state = 'elsewhere'), 0) \
             FROM jobs WHERE queued_at >= ?1",
        )?
        .query_row([since], |row| {
            Ok(sizing::LeftOut {
                queued: row.get(0)?,
                running: row.get(1)?,
                cancelled: row.get(2)?,
                elsewhere: row.get(3)?,
            })
        })?;
    Ok(sizing::History {
        jobs,
        cancelled,
        runners,
        kept_since,
        left_out,
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
    Ok(crate::insights::History {
        runs,
        kept_since,
        host: None,
        readings: host_readings(db, since)?,
    })
}

/// The host's readings the budget's events stored from `since` (ms), oldest first (ADR 0108). Only the figures the
/// insights read, by SQLite's JSON: a day holds thousands of these events. A runner ran while the budget counted more
/// than the mirror, seed containers and shells hold.
fn host_readings(
    db: &Connection,
    since: u64,
) -> rusqlite::Result<Vec<crate::insights::HostReading>> {
    db.prepare_cached(
        "SELECT at, json_extract(body, '$.budget.host.memory.total'), \
         json_extract(body, '$.budget.host.memory.available'), json_extract(body, '$.budget.host.memory.swap.free'), \
         coalesce(json_extract(body, '$.budget.host.credit.bytes'), 0), \
         json_extract(body, '$.budget.memory_in_use') > json_extract(body, '$.budget.mirror_memory') \
         + (SELECT coalesce(sum(json_extract(value, '$.memory')), 0) FROM json_each(body, '$.budget.seed_memory')) \
         + (SELECT coalesce(sum(json_extract(value, '$.memory')), 0) FROM json_each(body, '$.budget.shell_memory')) \
         FROM events WHERE at >= ?1 AND body LIKE '{\"type\":\"budget.changed\"%' \
         AND json_extract(body, '$.budget.host.memory.total') IS NOT NULL ORDER BY id",
    )?
    .query_map([since], |row| {
        Ok(crate::insights::HostReading {
            at: row.get(0)?,
            total: row.get(1)?,
            available: row.get(2)?,
            swap_free: row.get(3)?,
            credit: row.get(4)?,
            running: row.get(5)?,
        })
    })?
    .collect()
}

/// Those of `jobs` still queued, with how long each waited for the host's memory.
pub(super) fn held_jobs(
    db: &Connection,
    jobs: &BTreeSet<u64>,
) -> rusqlite::Result<Vec<crate::insights::HeldJob>> {
    let mut held = db.prepare_cached(
        "SELECT repository, workflow, name, host_waited FROM jobs WHERE id = ?1 AND state = 'queued'",
    )?;
    let mut found = Vec::new();
    for job in jobs {
        found.extend(
            held.query_row([job], |row| {
                Ok(crate::insights::HeldJob {
                    repository: row.get(0)?,
                    workflow: row.get(1)?,
                    name: row.get(2)?,
                    waited: row.get(3)?,
                })
            })
            .optional()?,
        );
    }
    Ok(found)
}

pub(super) fn warm(
    db: &Connection,
    since: u64,
) -> rusqlite::Result<Vec<crate::scheduler::warm::Seen>> {
    db.prepare_cached(
        "SELECT repository, workflow, run_id, run_attempt, queued_at, labels, finished_at, started_at FROM jobs \
         WHERE queued_at >= ?1 ORDER BY id",
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
            finished_at: row.get(6)?,
            started_at: row.get(7)?,
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

pub(super) fn last_runner_second(db: &Connection, prefix: &str) -> rusqlite::Result<Option<u64>> {
    // A name ends in its second: the digits after the last `-`. A job names its runner even when the runner never
    // got a row, as when Docker never let the engine inspect it. `runner_named` holds the latest second named, also
    // of runners pruning removed and of those nothing else recorded.
    db.prepare_cached(
        "SELECT max(second) FROM (
            SELECT CAST(substr(name, length(rtrim(name, '0123456789')) + 1) AS INTEGER) AS second FROM (
                SELECT name FROM runners WHERE substr(name, 1, length(?1)) = ?1
                UNION ALL
                SELECT runner AS name FROM jobs WHERE substr(runner, 1, length(?1)) = ?1
            )
            UNION ALL
            SELECT value FROM meta WHERE key = 'runner_named'
        )",
    )?
    .query_row([prefix], |row| row.get(0))
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
    let mut reseeded = BTreeMap::<String, u64>::new();
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

pub(super) fn runner_cancel_deadline(
    db: &Connection,
    runner: &str,
) -> rusqlite::Result<Option<u64>> {
    Ok(db
        .query_row(
            "SELECT cancel_by FROM runners WHERE name = ?1",
            [runner],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

pub(super) fn runner_memory_cap(db: &Connection, runner: &str) -> rusqlite::Result<Option<u64>> {
    Ok(db
        .query_row(
            "SELECT memory_cap FROM runners WHERE name = ?1",
            [runner],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
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

/// What a replay reads the slice's overhead from: instants with at least this much use by runners, in bytes. Under it
/// a reseed's container fills the slice and says nothing of what a busy slice holds beside its runners.
const BUSY_USE: u64 = 8 << 30;

/// The share of busy instants the overhead covers, in percent. It neither grows with the runners nor with their use on
/// the host studied (ADR 0099), so a replay carries one high figure.
const OVERHEAD_PERCENTILE: usize = 99;

/// The jobs of a trace.
struct Started {
    /// Each run with its samples, not named yet.
    runs: BTreeMap<RunId, RunTrace>,
    /// Each run and its start, by job ID.
    jobs: BTreeMap<u64, (RunId, u64)>,
}

/// The jobs that started from `since` (ms) on a runner, each run's samples counted from its job's start.
fn started_since(db: &Connection, since: u64) -> rusqlite::Result<Started> {
    let mut runs = BTreeMap::new();
    let mut jobs = BTreeMap::new();
    let mut statement = db.prepare(
        "SELECT id, run_id, run_attempt, name, started_at, runner FROM jobs
         WHERE started_at >= ?1 AND runner IS NOT NULL",
    )?;
    let rows = statement.query_map([since], |row| {
        Ok((
            row.get::<_, u64>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, u32>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut of_runner =
        db.prepare("SELECT at, memory FROM samples WHERE runner = ?1 ORDER BY at")?;
    for row in rows {
        let (job, run_id, run_attempt, name, started_at, runner) = row?;
        let samples = of_runner
            .query_map([&runner], |row| {
                Ok(Point {
                    at: row.get::<_, u64>(0)?.saturating_sub(started_at),
                    memory: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<Point>>>()?;
        let id = RunId {
            run_id,
            run_attempt,
            name,
        };
        jobs.insert(job, (id.clone(), started_at));
        runs.insert(
            id,
            RunTrace {
                samples,
                named_after: u64::MAX,
            },
        );
    }
    Ok(Started { runs, jobs })
}

/// How the jobs that started from `since` (ms) ran (`store::trace_at`). A job counts as named `named_by` ms after it
/// started, the next plan's reading, if its queued or started event had its name; else at the first event of its that
/// GitHub's details made, a step or its end.
pub(super) fn trace(
    db: &Connection,
    since: u64,
    named_by: u64,
    overhead: Option<u64>,
) -> rusqlite::Result<Trace> {
    let Started { mut runs, jobs } = started_since(db, since)?;
    let used = busy_use(db, since)?;
    let mut host_overheads = Vec::new();
    let mut outside = Vec::new();
    let mut total = 0;
    let mut events = db.prepare("SELECT at, body FROM events WHERE at >= ?1 ORDER BY id")?;
    // A job that starts in the window may have queued a while before it.
    let bodies = events.query_map([since.saturating_sub(DAY_MS)], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in bodies {
        let (at, body) = row?;
        let Ok(event) = serde_json::from_str::<api::Event>(&body) else {
            continue;
        };
        let named = match event {
            api::Event::BudgetChanged { budget } => {
                if let Some(host) = budget.host.filter(|_| at >= since) {
                    total = host.memory.total;
                    if let Some(used) = used.get(&(host.memory.at * 1000)) {
                        host_overheads.push(host.memory.slice.saturating_sub(*used));
                    }
                    outside.push(Outside {
                        at,
                        memory: host.memory.outside(),
                    });
                }
                continue;
            }
            api::Event::JobQueued { job } | api::Event::JobStarted { job } => {
                (!job.name.is_empty()).then_some((job.id, 0))
            }
            api::Event::JobStep { job, .. } => Some((job, at)),
            api::Event::JobFinished { job } => Some((job.id, at)),
            _ => None,
        };
        let Some((job, at)) = named else {
            continue;
        };
        if let Some((id, started_at)) = jobs.get(&job)
            && let Some(run) = runs.get_mut(id)
        {
            let after = at.saturating_sub(*started_at).max(named_by);
            run.named_after = run.named_after.min(after);
        }
    }
    for run in runs.values_mut() {
        if run.named_after == u64::MAX {
            run.named_after = named_by;
        }
    }
    let overhead = match overhead {
        Some(overhead) => overhead,
        None => measured_overhead(db, &used, since)?,
    };
    Ok(Trace {
        runs,
        outside,
        total,
        overhead,
        guard_overhead: overhead,
        host_overhead: high_percentile(host_overheads),
    })
}

/// What the runners' samples add up to at each busy second since `since` (ms), by that second in ms.
fn busy_use(db: &Connection, since: u64) -> rusqlite::Result<BTreeMap<u64, u64>> {
    let mut statement = db.prepare(
        "SELECT at / 1000 * 1000, SUM(memory) FROM samples WHERE at >= ?1 GROUP BY 1 HAVING SUM(memory) >= ?2",
    )?;
    statement
        .query_map([since, BUSY_USE], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// `values` at `OVERHEAD_PERCENTILE`; 0 with none.
fn high_percentile(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    let at = values.len() * OVERHEAD_PERCENTILE / 100;
    values.get(at).copied().unwrap_or(0)
}

/// What the slice held beside its runners' samples at the busy seconds of `used` since `since` (ms).
fn measured_overhead(
    db: &Connection,
    used: &BTreeMap<u64, u64>,
    since: u64,
) -> rusqlite::Result<u64> {
    let mut statement = db.prepare("SELECT at, memory FROM host_samples WHERE at >= ?1")?;
    let samples = statement.query_map([since], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?))
    })?;
    let mut overheads = Vec::new();
    for sample in samples {
        let (at, memory) = sample?;
        if let Some(used) = used.get(&at) {
            overheads.push(memory.saturating_sub(*used));
        }
    }
    Ok(high_percentile(overheads))
}
