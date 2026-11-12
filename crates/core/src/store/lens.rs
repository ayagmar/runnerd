//! Reads for the job history's two lenses (ADR 0135): a window's timeline of slots, and a job beside its baseline.
//! Each response is read in one snapshot ([`read::snapshot`]) and within its bounds at the read: at most
//! [`api::TIMELINE_BARS_MAX`] bars and runners, names cut before they leave the database. Reads only; what the rows
//! mean is `api`'s.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::read::{self, secs, state_name};
use crate::api::{
    self, BarEnd, BaselineRead, JobComparison, JobState, RunnerSpan, SlotKey, SlotTimeline,
    SlottedBar, SlottedRunner, Span, StepUsage, TimelineBar, TimelineRows, Unavailable,
};

/// Host samples further apart than this many sample intervals leave the time between them unobserved.
const OBSERVED_GAP_INTERVALS: u64 = 3;

/// The most spans of observation a timeline reads; past them, the rest of the window reads as not observed.
const OBSERVED_MAX: usize = 1000;

fn ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

fn end(at: Option<u64>, open: bool) -> BarEnd {
    match (at, open) {
        (Some(at), _) => BarEnd::At { at: secs(at) },
        (None, true) => BarEnd::Open,
        (None, false) => BarEnd::Unknown,
    }
}

/// The states whose records may still change, in SQL.
const OPEN: &str = "('queued', 'running')";

/// A job's bar: the columns `TIMELINE_JOBS` reads.
fn bar(row: &Row<'_>) -> rusqlite::Result<SlottedBar> {
    let state = read::state(&row.get::<_, String>(4)?);
    let profile: Option<String> = row.get(11)?;
    let slot: Option<usize> = row.get(12)?;
    let runner: Option<String> = row.get(10)?;
    Ok(SlottedBar {
        bar: TimelineBar {
            job: row.get(0)?,
            repository: row.get(1)?,
            workflow: row.get(2)?,
            name: row.get(3)?,
            state,
            conclusion: row.get(5)?,
            runner_fault: row.get(6)?,
            queued_at: secs(row.get(7)?),
            started_at: row.get::<_, Option<u64>>(8)?.map(secs),
            end: end(row.get(9)?, !state.is_final()),
            runner: runner.clone().filter(|_| profile.is_some()),
        },
        slot: profile
            .zip(slot)
            .filter(|_| runner.is_some())
            .map(|(profile, slot)| SlotKey { profile, slot }),
    })
}

/// The timeline of `window` (seconds); `held` are the host samples not yet stored, in seconds.
pub(super) fn timeline(
    db: &Connection,
    window: Span,
    held: &[u64],
    interval: u64,
) -> rusqlite::Result<SlotTimeline> {
    read::snapshot(db, |db| {
        let (from, to) = (ms(window.from), ms(window.to));
        let cap = api::TIMELINE_BARS_MAX + 1;
        // Mirrors `TimelineBar::overlaps`: an end in the window or after it, an open job, or one whose end is unknown
        // that began in the window.
        let jobs = db
            .prepare_cached(&format!(
                "SELECT j.id, j.repository, substr(j.workflow, 1, ?4), substr(j.name, 1, ?4), j.state, \
                 j.conclusion, j.runner_fault, j.queued_at, j.started_at, j.finished_at, j.runner, r.profile, \
                 r.slot FROM jobs j LEFT JOIN runners r ON r.name = j.runner \
                 WHERE j.queued_at < ?2 AND (j.finished_at / 1000 > ?5 OR (j.finished_at IS NULL \
                 AND (j.state IN {OPEN} OR coalesce(j.started_at, j.queued_at) >= ?1))) \
                 ORDER BY j.queued_at / 1000 DESC, j.id DESC LIMIT ?3"
            ))?
            .query_map(
                params![from, to, cap, api::TITLE_SOURCE_MAX, window.from],
                bar,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let runners = db
            .prepare_cached(
                "SELECT name, profile, slot, started_at, removed_at FROM runners \
                 WHERE started_at < ?2 AND (removed_at IS NULL OR removed_at / 1000 > ?4) \
                 ORDER BY started_at / 1000 DESC, name DESC LIMIT ?3",
            )?
            .query_map(params![from, to, cap, window.from], |row| {
                Ok(SlottedRunner {
                    span: RunnerSpan {
                        name: row.get(0)?,
                        started_at: secs(row.get(3)?),
                        end: end(row.get(4)?, true),
                    },
                    slot: SlotKey {
                        profile: row.get(1)?,
                        slot: row.get(2)?,
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let history_from = secs(read::kept_since(db)?);
        Ok(SlotTimeline::assemble(TimelineRows {
            window: Some(window),
            jobs,
            runners,
            observed: observed(db, window, history_from, held, interval)?,
            history_from,
        }))
    })
}

/// When runnerd was watching in `window`, from `history_from` on: the host samples, each no more than
/// [`OBSERVED_GAP_INTERVALS`] intervals after the one before, stored or `held`. Only the samples that start a
/// span are read.
fn observed(
    db: &Connection,
    window: Span,
    history_from: u64,
    held: &[u64],
    interval: u64,
) -> rusqlite::Result<Vec<Span>> {
    let gap = interval.saturating_mul(OBSERVED_GAP_INTERVALS).max(1);
    let from = window.from.max(history_from);
    if from >= window.to {
        return Ok(Vec::new());
    }
    // Each row starts a span: the first sample, and each after a gap, with the sample before it.
    let starts: Vec<(u64, Option<u64>)> = db
        .prepare_cached(
            "SELECT at, before FROM (SELECT at, lag(at) OVER (ORDER BY at) AS before FROM host_samples \
             WHERE at >= ?1 AND at <= ?2) WHERE before IS NULL OR at - before > ?3 ORDER BY at LIMIT ?4",
        )?
        .query_map(
            params![ms(from), ms(window.to), ms(gap), OBSERVED_MAX + 1],
            |row| Ok((secs(row.get(0)?), row.get::<_, Option<u64>>(1)?.map(secs))),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let last: Option<u64> = db
        .query_row(
            "SELECT max(at) FROM host_samples WHERE at >= ?1 AND at <= ?2",
            params![ms(from), ms(window.to)],
            |row| row.get::<_, Option<u64>>(0),
        )?
        .map(secs);
    let mut spans: Vec<Span> = Vec::new();
    for (at, before) in starts.iter().take(OBSERVED_MAX) {
        if let (Some(span), Some(before)) = (spans.last_mut(), before) {
            span.to = *before;
        }
        spans.push(Span { from: *at, to: *at });
    }
    if starts.len() <= OBSERVED_MAX {
        if let (Some(span), Some(last)) = (spans.last_mut(), last) {
            span.to = last;
        }
        // The samples held, not yet stored, carry on from the last stored one or start their own.
        let stored = last.unwrap_or(0);
        for at in held
            .iter()
            .copied()
            .filter(|at| *at > stored && (from..=window.to).contains(at))
        {
            match spans.last_mut() {
                Some(span) if at.saturating_sub(span.to) <= gap => span.to = at,
                _ => spans.push(Span { from: at, to: at }),
            }
        }
    }
    spans.retain(|span| span.to > span.from);
    Ok(spans)
}

/// `job` beside the job `with` names, or its baseline; `None` without `job`, or without the job `with` names.
pub(super) fn compare(
    db: &Connection,
    job: u64,
    with: Option<u64>,
) -> rusqlite::Result<Option<JobComparison>> {
    read::snapshot(db, |db| {
        let Some(this) = read::job(db, job)? else {
            return Ok(None);
        };
        let usage = step_usage(db, &this)?;
        let other = match with {
            Some(with) => match read::job(db, with)? {
                None => return Ok(None),
                Some(other) if !JobComparison::same_job(&this.summary, &other.summary) => {
                    BaselineRead::Incompatible(with)
                }
                Some(other) => BaselineRead::Found {
                    usage: step_usage(db, &other)?,
                    job: Box::new(other),
                    asked: true,
                },
            },
            None => match baseline(db, &this.summary)? {
                Ok(id) => match read::job(db, id)? {
                    Some(other) => BaselineRead::Found {
                        usage: step_usage(db, &other)?,
                        job: Box::new(other),
                        asked: false,
                    },
                    None => BaselineRead::Unavailable(Unavailable::NoEarlierSuccess),
                },
                Err(reason) => BaselineRead::Unavailable(reason),
            },
        };
        Ok(Some(JobComparison::of(&this, &usage, other)))
    })
}

/// The ID of `job`'s baseline, as [`JobComparison::baseline_of`] picks it.
fn baseline(db: &Connection, job: &api::JobSummary) -> rusqlite::Result<Result<u64, Unavailable>> {
    let (Some(workflow), Some(branch)) = (&job.workflow, &job.branch) else {
        return Ok(Err(if job.workflow.is_none() {
            Unavailable::UnknownWorkflow
        } else {
            Unavailable::UnknownBranch
        }));
    };
    // `queued_at` is compared in whole seconds, as `is_baseline` compares the API's.
    let found: Option<u64> = db
        .prepare_cached(&format!(
            "SELECT id FROM jobs WHERE repository = ?1 AND workflow = ?2 AND name = ?3 AND branch = ?4 \
             AND state = '{}' AND conclusion = 'success' AND runner IS NOT NULL \
             AND EXISTS (SELECT 1 FROM runners r WHERE r.name = jobs.runner) AND started_at IS NOT NULL \
             AND (queued_at / 1000 < ?5 OR (queued_at / 1000 = ?5 AND id < ?6)) AND id != ?6 \
             ORDER BY queued_at / 1000 DESC, id DESC LIMIT 1",
            state_name(JobState::Completed)
        ))?
        .query_row(
            params![
                job.repository,
                workflow,
                job.name,
                branch,
                i64::try_from(job.timeline.queued_at).unwrap_or(i64::MAX),
                i64::try_from(job.id).unwrap_or(i64::MAX)
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.ok_or(Unavailable::NoEarlierSuccess))
}

/// What `job`'s runner's samples say over each of its steps with both times: the samples after its start up to its
/// end, as `SampleSeries::usage_between` reads them. None once the samples were pruned.
fn step_usage(db: &Connection, job: &api::Job) -> rusqlite::Result<Vec<StepUsage>> {
    let Some(runner) = &job.summary.runner else {
        return Ok(Vec::new());
    };
    let mut usage = Vec::new();
    let mut statement = db.prepare_cached(
        "SELECT count(*), max(memory), sum(cpu) FROM samples WHERE runner = ?1 AND at > ?2 AND at <= ?3",
    )?;
    for step in &job.steps {
        let (Some(from), Some(to)) = (step.started_at, step.completed_at) else {
            continue;
        };
        let (samples, memory, cpu): (u64, Option<u64>, Option<u64>) = statement
            .query_row(params![runner.name, ms(from), ms(to)], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if let (Some(memory_peak), Some(cpu)) = (memory, cpu)
            && samples > 0
        {
            usage.push(StepUsage {
                number: step.number,
                samples,
                memory_peak,
                cpu_average: cpu / samples,
            });
        }
    }
    Ok(usage)
}
