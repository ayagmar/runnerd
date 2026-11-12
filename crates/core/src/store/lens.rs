//! Reads for the job history's two lenses (ADR 0135): a window's timeline of slots, and a job beside its baseline.
//! Each response is read in one snapshot ([`read::snapshot`]) and within its bounds at the read: at most
//! [`api::TIMELINE_BARS_MAX`] bars and runners, names cut before they leave the database. Reads only; what the rows
//! mean is `api`'s.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::read::{self, byte_prefix, secs, state_name, text};
use crate::api::{
    self, BarEnd, BaselineRead, JobComparison, JobState, RunnerSpan, SlotKey, SlotTimeline,
    SlottedBar, SlottedRunner, Span, StepUsage, TimelineBar, TimelineRows, Unavailable,
};

/// Host samples further apart than this many sample intervals leave the time between them unobserved: the
/// tolerance a step's samples are held to.
const OBSERVED_GAP_INTERVALS: u64 = api::SAMPLE_GAP_INTERVALS;

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
            workflow: text(row, 2)?,
            name: text(row, 3)?.unwrap_or_default(),
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
        // while its runner may have been up in the window, or without a runner here, that began in it.
        let jobs = db
            .prepare_cached(&format!(
                "SELECT j.id, j.repository, {}, {}, j.state, \
                 j.conclusion, j.runner_fault, j.queued_at, j.started_at, j.finished_at, j.runner, r.profile, \
                 r.slot FROM jobs j LEFT JOIN runners r ON r.name = j.runner \
                 WHERE j.queued_at < ?2 AND (j.finished_at / 1000 > ?4 OR (j.finished_at IS NULL \
                 AND (j.state IN {OPEN} OR (r.name IS NOT NULL AND (r.removed_at IS NULL OR r.removed_at / 1000 > ?4)) \
                 OR (r.name IS NULL AND coalesce(j.started_at, j.queued_at) >= ?1)))) \
                 ORDER BY j.queued_at / 1000 DESC, j.id DESC LIMIT ?3",
                byte_prefix("j.workflow", api::TITLE_SOURCE_MAX),
                byte_prefix("j.name", api::TITLE_SOURCE_MAX)
            ))?
            .query_map(
                params![from, to, cap, window.from],
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

/// When runnerd was watching in `window`, from `history_from` on: inside each daemon's session, where its host
/// samples are no more than [`OBSERVED_GAP_INTERVALS`] intervals apart, stored or `held`. A restart between two
/// samples is a gap however short; time outside every session, as before the first one a history recorded, is not
/// watched. Only the samples that start a span are read.
fn observed(
    db: &Connection,
    window: Span,
    history_from: u64,
    held: &[u64],
    interval: u64,
) -> rusqlite::Result<Vec<Span>> {
    let from = window.from.max(history_from);
    if from >= window.to {
        return Ok(Vec::new());
    }
    // The latest session is this daemon's: it goes on past its last stored sample, with the samples it holds.
    let sessions: Vec<(Span, bool)> = db
        .prepare_cached(
            "SELECT started_at, coalesce(sampled_at, started_at), id = (SELECT max(id) FROM sessions) \
             FROM sessions WHERE started_at <= ?2 AND (coalesce(sampled_at, started_at) >= ?1 \
             OR id = (SELECT max(id) FROM sessions)) ORDER BY started_at LIMIT ?3",
        )?
        .query_map(params![ms(from), ms(window.to), OBSERVED_MAX], |row| {
            let latest: bool = row.get(2)?;
            let span = Span {
                from: secs(row.get(0)?).max(from),
                to: if latest {
                    window.to
                } else {
                    secs(row.get(1)?).min(window.to)
                },
            };
            Ok((span, latest))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut spans = Vec::new();
    for (session, latest) in sessions {
        if session.to > session.from {
            let held = if latest { held } else { &[] };
            spans.extend(sampled(db, session, held, interval)?);
        }
    }
    spans.truncate(OBSERVED_MAX);
    Ok(spans)
}

/// The stretches of `window`, a session's, with host samples no more than [`OBSERVED_GAP_INTERVALS`] intervals
/// apart, stored or `held`.
fn sampled(
    db: &Connection,
    window: Span,
    held: &[u64],
    interval: u64,
) -> rusqlite::Result<Vec<Span>> {
    let gap = interval.saturating_mul(OBSERVED_GAP_INTERVALS).max(1);
    // Each row starts a span: the first sample, and each after a gap, with the sample before it.
    let starts: Vec<(u64, Option<u64>)> = db
        .prepare_cached(
            "SELECT at, before FROM (SELECT at, lag(at) OVER (ORDER BY at) AS before FROM host_samples \
             WHERE at >= ?1 AND at <= ?2) WHERE before IS NULL OR at - before > ?3 ORDER BY at LIMIT ?4",
        )?
        .query_map(
            params![ms(window.from), ms(window.to), ms(gap), OBSERVED_MAX + 1],
            |row| Ok((secs(row.get(0)?), row.get::<_, Option<u64>>(1)?.map(secs))),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let last: Option<u64> = db
        .query_row(
            "SELECT max(at) FROM host_samples WHERE at >= ?1 AND at <= ?2",
            params![ms(window.from), ms(window.to)],
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
            .filter(|at| *at > stored && (window.from..=window.to).contains(at))
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

/// `job` beside the job `with` names, or its baseline; `None` without `job`, or without the job `with` names. The
/// baseline is picked from the summaries; steps, bounded as [`api::ComparedSteps`] reads them, and samples are read
/// only once there is one.
pub(super) fn compare(
    db: &Connection,
    job: u64,
    with: Option<u64>,
) -> rusqlite::Result<Option<JobComparison>> {
    read::snapshot(db, |db| {
        let Some(this) = read::summary(db, job)? else {
            return Ok(None);
        };
        let (other, asked) = match with {
            Some(with) => match read::summary(db, with)? {
                None => return Ok(None),
                Some(other) if !JobComparison::same_job(&this, &other) => {
                    return Ok(Some(JobComparison::of(
                        &this,
                        &api::ComparedSteps::default(),
                        &[],
                        BaselineRead::Incompatible(with),
                    )));
                }
                Some(other) => (other, true),
            },
            None => match baseline(db, &this)? {
                Ok(id) => match read::summary(db, id)? {
                    Some(other) => (other, false),
                    None => return Ok(Some(alone(&this, Unavailable::NoEarlierSuccess))),
                },
                Err(reason) => return Ok(Some(alone(&this, reason))),
            },
        };
        let steps = compared_steps(db, this.id)?;
        let usage = step_usage(db, &this, &steps.steps)?;
        let other_steps = compared_steps(db, other.id)?;
        let found = BaselineRead::Found {
            usage: step_usage(db, &other, &other_steps.steps)?,
            steps: other_steps,
            job: Box::new(other),
            asked,
        };
        Ok(Some(JobComparison::of(&this, &steps, &usage, found)))
    })
}

/// `job` with no baseline, for `reason`.
fn alone(job: &api::JobSummary, reason: Unavailable) -> JobComparison {
    JobComparison::of(
        job,
        &api::ComparedSteps::default(),
        &[],
        BaselineRead::Unavailable(reason),
    )
}

/// Job `job`'s steps as a comparison reads them: in order, each name cut to its first bytes before it leaves the
/// database, one past [`api::COMPARE_STEPS_MAX`] read to tell whether there are more.
fn compared_steps(db: &Connection, job: u64) -> rusqlite::Result<api::ComparedSteps> {
    let mut statement = db.prepare_cached(&format!(
        "SELECT number, {}, {}, {}, started_at, completed_at FROM steps WHERE job = ?1 ORDER BY number \
         LIMIT ?2",
        byte_prefix("name", api::TITLE_SOURCE_MAX),
        byte_prefix("status", STEP_WORD_MAX),
        byte_prefix("conclusion", STEP_WORD_MAX),
    ))?;
    let limit = i64::try_from(api::COMPARE_STEPS_MAX + 1).unwrap_or(i64::MAX);
    let mut rows = statement.query(params![job, limit])?;
    let mut read = api::ComparedSteps::default();
    while let Some(row) = rows.next()? {
        let step = api::Step {
            number: row.get(0)?,
            name: text(row, 1)?.unwrap_or_default(),
            status: text(row, 2)?.unwrap_or_default(),
            conclusion: text(row, 3)?,
            started_at: row.get::<_, Option<u64>>(4)?.map(secs),
            completed_at: row.get::<_, Option<u64>>(5)?.map(secs),
        };
        if !read.push(step) {
            read.cut = true;
            break;
        }
    }
    Ok(read)
}

/// The most bytes of a step's status or conclusion a comparison reads: GitHub's words are short.
const STEP_WORD_MAX: usize = 64;

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

/// What the samples of one step's window add up to.
struct Sampled {
    count: u64,
    memory: Option<u64>,
    cpu: Option<u64>,
    first: Option<u64>,
    last: Option<u64>,
    widest_gap: Option<u64>,
}

/// What `job`'s runner's samples say over each of `steps` with both times: the samples after its start up to its
/// end, as `SampleSeries::usage_between` reads them. None once the samples were pruned.
fn step_usage(
    db: &Connection,
    job: &api::JobSummary,
    steps: &[api::Step],
) -> rusqlite::Result<Vec<StepUsage>> {
    let Some(runner) = &job.runner else {
        return Ok(Vec::new());
    };
    let mut usage = Vec::new();
    // The samples of the seconds after the step's start up to its end, as the API's seconds count them.
    let mut statement = db.prepare_cached(
        "SELECT count(*), max(memory), sum(cpu), min(at) / 1000, max(at) / 1000, max(gap) FROM \
         (SELECT at, memory, cpu, at / 1000 - lag(at / 1000) OVER (ORDER BY at) AS gap FROM samples \
         WHERE runner = ?1 AND at >= ?2 AND at < ?3)",
    )?;
    for step in steps {
        let (Some(from), Some(to)) = (step.started_at, step.completed_at) else {
            continue;
        };
        let read = statement.query_row(params![runner.name, ms(from + 1), ms(to + 1)], |row| {
            Ok(Sampled {
                count: row.get(0)?,
                memory: row.get(1)?,
                cpu: row.get(2)?,
                first: row.get(3)?,
                last: row.get(4)?,
                widest_gap: row.get(5)?,
            })
        })?;
        if let (Some(memory_peak), Some(cpu), Some(first), Some(last)) =
            (read.memory, read.cpu, read.first, read.last)
            && read.count > 0
        {
            usage.push(StepUsage {
                number: step.number,
                first,
                last,
                widest_gap: read.widest_gap.unwrap_or(0),
                memory_peak,
                cpu_average: cpu / read.count,
            });
        }
    }
    Ok(usage)
}
