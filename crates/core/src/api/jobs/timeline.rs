//! `GET /v1/timeline` (ADR 0135): what each slot ran over a window, from the history alone. A job's bar sits in the
//! lane of the profile and slot its runner had when it ran, whatever the config holds now; a job no runner of this
//! daemon took waits in the unassigned lane. Bars keep their real ends, past the window too, and an end the history
//! does not have is said to be unknown. Where the history cannot show that runnerd was watching, a lane's empty
//! stretch is unknown, never idle. No reservation is drawn: the history keeps no series of them.
//!
//! Pure: the store reads the rows ([`TimelineRows`]) and [`SlotTimeline::assemble`] lays them out, as the demo's
//! fake daemon and the fixtures do from jobs in memory.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::facts::outcome_of;
use super::log::title;
use super::{JobState, JobSummary, JobTitle, Outcome, job_title};

/// The window a timeline shows unless `since` says otherwise: the last 24 hours, in seconds.
pub const TIMELINE_DEFAULT: u64 = 24 * 3600;

/// The longest window a timeline shows, in seconds: 7 days, the samples the history keeps by default.
pub const TIMELINE_WINDOW_MAX: u64 = 7 * 24 * 3600;

/// The most job bars a timeline holds, and the most runner spans: past it, the latest are kept and
/// [`SlotTimeline::cut_before`] says from when.
pub const TIMELINE_BARS_MAX: usize = 2000;

/// `GET /v1/timeline` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineQuery {
    /// The window's start; default [`TIMELINE_DEFAULT`] before its end.
    pub since: Option<u64>,
    /// The window's end; default and at most now.
    pub until: Option<u64>,
}

impl TimelineQuery {
    /// The window it asks for at `now`, or why the daemon refuses it: a start after its end, or a window longer than
    /// [`TIMELINE_WINDOW_MAX`].
    pub fn window(&self, now: u64) -> Result<Span, String> {
        let to = self.until.unwrap_or(now).min(now);
        let from = self
            .since
            .unwrap_or_else(|| to.saturating_sub(TIMELINE_DEFAULT));
        if from >= to {
            return Err("a timeline's since is before its until".to_owned());
        }
        if to - from > TIMELINE_WINDOW_MAX {
            return Err(format!(
                "a timeline shows {} at most",
                crate::api::duration(TIMELINE_WINDOW_MAX)
            ));
        }
        Ok(Span { from, to })
    }
}

/// A stretch of time, in seconds since the epoch: from `from` up to `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Its start.
    pub from: u64,
    /// Its end.
    pub to: u64,
}

/// How a bar or a runner's span ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BarEnd {
    /// It ended then, maybe after the window.
    At {
        /// When.
        at: u64,
    },
    /// It goes on: the job runs or waits, the container is up.
    Open,
    /// It ended, and the history does not say when.
    Unknown,
    /// A newer daemon's end this version does not know.
    #[serde(other)]
    Other,
}

impl BarEnd {
    /// The end's time; `None` unless it is known.
    pub fn at(self) -> Option<u64> {
        match self {
            BarEnd::At { at } => Some(at),
            BarEnd::Open | BarEnd::Unknown | BarEnd::Other => None,
        }
    }
}

/// One job on a timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineBar {
    /// The job's ID.
    pub job: u64,
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name, without escapes or control characters, cut to 200 bytes.
    pub workflow: Option<String>,
    /// The job's name, as the workflow's.
    pub name: String,
    /// Where the job is.
    pub state: JobState,
    /// GitHub's conclusion once completed.
    pub conclusion: Option<String>,
    /// Whether the runner failed it rather than its code.
    pub runner_fault: bool,
    /// When GitHub queued it: its wait starts here.
    pub queued_at: u64,
    /// When its runner started it, which ends its wait; `None` until then, or when the history does not have it.
    pub started_at: Option<u64>,
    /// How it ended: for a job that started, its run; for one that did not, its wait.
    pub end: BarEnd,
    /// Its runner's name, in a slot's lane.
    pub runner: Option<String>,
}

impl TimelineBar {
    /// A job's bar as the history holds it; its names are made safe in [`SlotTimeline::assemble`].
    pub fn of(job: &JobSummary) -> TimelineBar {
        let end = match (job.timeline.finished_at, job.state.is_final()) {
            (Some(at), _) => BarEnd::At { at },
            (None, false) => BarEnd::Open,
            (None, true) => BarEnd::Unknown,
        };
        TimelineBar {
            job: job.id,
            repository: job.repository.clone(),
            workflow: job.workflow.clone(),
            name: job.name.clone(),
            state: job.state,
            conclusion: job.conclusion.clone(),
            runner_fault: job.runner_fault,
            queued_at: job.timeline.queued_at,
            started_at: job.timeline.started_at,
            end,
            runner: job.runner.as_ref().map(|runner| runner.name.clone()),
        }
    }

    /// Its outcome, as the job's page says it.
    pub fn outcome(&self) -> Outcome {
        outcome_of(
            self.state,
            self.conclusion.as_deref(),
            self.started_at.is_some(),
            self.runner_fault,
        )
    }

    /// Its name as every client writes a job's.
    pub fn title(&self) -> JobTitle<'_> {
        job_title(self.workflow.as_deref(), &self.name, Some(self.job))
    }

    /// What its end says when it has no time: `running`, `waiting`, or `end unknown`; `None` for one that ended
    /// when it says.
    pub fn end_words(&self) -> Option<&'static str> {
        match (self.end, self.started_at) {
            (BarEnd::At { .. }, _) => None,
            (BarEnd::Open, Some(_)) => Some("running"),
            (BarEnd::Open, None) => Some("waiting"),
            (BarEnd::Unknown | BarEnd::Other, _) => Some("end unknown"),
        }
    }

    /// When its run began, its key in a lane: its start, or for a job that did not start, its queueing.
    fn begins(&self) -> u64 {
        self.started_at.unwrap_or(self.queued_at)
    }

    /// Whether it belongs on a timeline of `window`: queued before its end, and ended after its start, or still
    /// going on. A job whose end is unknown may have run until its runner, which ended as `runner` says, was
    /// removed: it is shown while that runner may have been up in the window. Without a runner here, only when it
    /// began in the window. The store reads the same.
    pub fn overlaps(&self, window: Span, runner: Option<BarEnd>) -> bool {
        self.queued_at < window.to
            && match (self.end, runner) {
                (BarEnd::At { at }, _) => at > window.from,
                (BarEnd::Open, _) => true,
                (BarEnd::Unknown | BarEnd::Other, Some(runner)) => {
                    runner.at().is_none_or(|at| at > window.from)
                }
                (BarEnd::Unknown | BarEnd::Other, None) => self.begins() >= window.from,
            }
    }
}

/// A runner container in a slot: up from its start to its removal, with a job or without, as a warm runner waits.
/// Its life is not its job's run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerSpan {
    /// The runner's name, also its container's.
    pub name: String,
    /// When its container started.
    pub started_at: u64,
    /// When it was removed.
    pub end: BarEnd,
}

/// One slot as it was: the profile and slot its runners had when they ran, whatever the config holds now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    /// The profile.
    pub profile: String,
    /// The slot, 1-based.
    pub slot: usize,
    /// Its jobs, by when they began, oldest first.
    pub bars: Vec<TimelineBar>,
    /// Its runners' containers, oldest first.
    pub runners: Vec<RunnerSpan>,
}

/// What a lane's stretch without a job means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    /// runnerd was watching, and no job ran in the slot.
    NoJob,
    /// The history cannot say: runnerd was not watching, its samples were pruned, or a bar's start or end is
    /// unknown.
    Unknown,
}

impl GapKind {
    /// Its words: `no job` or `unknown`.
    pub fn words(self) -> &'static str {
        match self {
            GapKind::NoJob => "no job",
            GapKind::Unknown => "unknown",
        }
    }
}

/// A stretch of a lane without a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// When.
    pub span: Span,
    /// What it means.
    pub kind: GapKind,
}

/// `GET /v1/timeline`: what each slot ran over a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotTimeline {
    /// The window's start.
    pub since: u64,
    /// The window's end.
    pub until: u64,
    /// One per profile and slot a runner had in the window, by profile, then slot.
    pub lanes: Vec<Lane>,
    /// Jobs no runner of this daemon took, by when they were queued: waiting, ended before a runner took them, or run
    /// elsewhere.
    pub unassigned: Vec<TimelineBar>,
    /// When runnerd was watching, as its host samples show, oldest first; never before `history_from`.
    pub observed: Vec<Span>,
    /// Since when the history holds every job that ended: before it, pruning may have taken some.
    pub history_from: u64,
    /// The window held more than [`TIMELINE_BARS_MAX`] bars or runners: some that began at or before this time are
    /// left out. `None` when none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_before: Option<u64>,
}

/// What the store reads for a timeline, before [`SlotTimeline::assemble`] lays it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimelineRows {
    /// The window.
    pub window: Option<Span>,
    /// The jobs that overlap it, each with the slot its runner had, if any.
    pub jobs: Vec<SlottedBar>,
    /// The runner containers that overlap it.
    pub runners: Vec<SlottedRunner>,
    /// When runnerd was watching.
    pub observed: Vec<Span>,
    /// Since when the history holds every job that ended.
    pub history_from: u64,
}

/// A job's bar and the slot its runner had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlottedBar {
    /// The bar.
    pub bar: TimelineBar,
    /// The profile and slot, or `None` without a runner here.
    pub slot: Option<SlotKey>,
}

/// A runner container's span and its slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlottedRunner {
    /// The span.
    pub span: RunnerSpan,
    /// Its profile and slot.
    pub slot: SlotKey,
}

/// A profile's slot, as a runner had it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotKey {
    /// The profile.
    pub profile: String,
    /// 1-based.
    pub slot: usize,
}

impl SlotTimeline {
    /// What a timeline that left bars out says, read at `now`: `more than 2000 jobs and runners in this window: jobs
    /// that began before 5h 00m ago are left out`. `None` when it left none out.
    pub fn cut_line(&self, now: u64) -> Option<String> {
        let cut = self.cut_before?;
        Some(format!(
            "more than {TIMELINE_BARS_MAX} jobs and runners in this window: jobs that began before {} are left out",
            crate::api::ago(cut, now)
        ))
    }

    /// Lays out what the store read: jobs into their slot's lane or the unassigned one, the latest
    /// [`TIMELINE_BARS_MAX`] bars and runners kept, names made safe.
    pub fn assemble(rows: TimelineRows) -> SlotTimeline {
        let window = rows.window.unwrap_or(Span { from: 0, to: 0 });
        let mut jobs = rows.jobs;
        jobs.sort_by_key(|job| std::cmp::Reverse((job.bar.queued_at, job.bar.job)));
        let mut runners = rows.runners;
        runners.sort_by(|a, b| {
            (b.span.started_at, &b.span.name).cmp(&(a.span.started_at, &a.span.name))
        });
        let mut cut_before = None;
        if jobs.len() > TIMELINE_BARS_MAX {
            jobs.truncate(TIMELINE_BARS_MAX);
            cut_before = jobs.last().map(|job| job.bar.queued_at);
        }
        if runners.len() > TIMELINE_BARS_MAX {
            runners.truncate(TIMELINE_BARS_MAX);
            let cut = runners.last().map(|runner| runner.span.started_at);
            cut_before = cut_before.max(cut);
        }
        let mut lanes: BTreeMap<SlotKey, Lane> = BTreeMap::new();
        let mut unassigned = Vec::new();
        for job in jobs.into_iter().rev() {
            let mut bar = job.bar;
            bar.name = title(&bar.name);
            bar.workflow = bar.workflow.as_deref().map(title);
            match job.slot {
                Some(key) => lane(&mut lanes, key).bars.push(bar),
                None => unassigned.push(bar),
            }
        }
        for runner in runners.into_iter().rev() {
            lane(&mut lanes, runner.slot).runners.push(runner.span);
        }
        for lane in lanes.values_mut() {
            lane.bars
                .sort_by_key(|bar| (bar.begins(), bar.queued_at, bar.job));
        }
        SlotTimeline {
            since: window.from,
            until: window.to,
            lanes: lanes.into_values().collect(),
            unassigned,
            observed: rows.observed,
            history_from: rows.history_from,
            cut_before,
        }
    }

    /// A timeline of `jobs`, a whole history held in memory, over `window`, with runnerd watching over `observed`:
    /// for the demo's fake daemon and the fixtures. Runner containers are those of the jobs.
    pub fn of(
        jobs: &[JobSummary],
        window: Span,
        observed: Vec<Span>,
        history_from: u64,
    ) -> SlotTimeline {
        let mut rows = TimelineRows {
            window: Some(window),
            observed,
            history_from,
            ..TimelineRows::default()
        };
        for job in jobs {
            let bar = TimelineBar::of(job);
            // Its runner's removal ends its container's span.
            let runner_end = job.runner.as_ref().map(|_| {
                match (job.timeline.cleaned_at, job.state.is_final()) {
                    (Some(at), _) => BarEnd::At { at },
                    (None, false) => BarEnd::Open,
                    (None, true) => BarEnd::Unknown,
                }
            });
            if !bar.overlaps(window, runner_end) {
                continue;
            }
            let slot = job.runner.as_ref().map(|runner| SlotKey {
                profile: runner.profile.clone(),
                slot: runner.slot,
            });
            if let (Some(runner), Some(slot), Some(end)) = (&job.runner, &slot, runner_end) {
                let started_at = job.timeline.container_started_at.unwrap_or(bar.queued_at);
                let up = started_at < window.to && end.at().is_none_or(|at| at > window.from);
                if up {
                    rows.runners.push(SlottedRunner {
                        span: RunnerSpan {
                            name: runner.name.clone(),
                            started_at,
                            end,
                        },
                        slot: slot.clone(),
                    });
                }
            }
            rows.jobs.push(SlottedBar { bar, slot });
        }
        SlotTimeline::assemble(rows)
    }
}

impl Lane {
    /// Its stretches without a job in `timeline`'s window, each worded by core: `no job` only where runnerd was
    /// watching and the history is whole; `unknown` where it was not watching, where a bar's start or end is
    /// unknown (until its runner was removed), and everywhere once the timeline left bars out (`cut_before`), since
    /// one left out may have run in any lane. A bar's wait before its start is not a job in the slot.
    pub fn gaps(&self, timeline: &SlotTimeline) -> Vec<Gap> {
        let window = Span {
            from: timeline.since,
            to: timeline.until,
        };
        if timeline.cut_before.is_some() {
            return subtract(window, &self.busy(window))
                .into_iter()
                .map(|span| Gap {
                    span,
                    kind: GapKind::Unknown,
                })
                .collect();
        }
        // What the history cannot say about the slot: a bar without a start, or whose end is unknown until its
        // runner was removed.
        let removed = |bar: &TimelineBar| {
            self.runners
                .iter()
                .find(|runner| Some(&runner.name) == bar.runner.as_ref())
                .and_then(|runner| runner.end.at())
        };
        let mut unsure = Vec::new();
        for bar in &self.bars {
            match (bar.started_at, bar.end) {
                (Some(_), BarEnd::At { .. } | BarEnd::Open) => {}
                (Some(from), BarEnd::Unknown | BarEnd::Other) => unsure.push(Span {
                    from,
                    to: removed(bar).unwrap_or(window.to),
                }),
                (None, end) => unsure.push(Span {
                    from: bar.queued_at,
                    to: end.at().unwrap_or(window.to),
                }),
            }
        }
        let mut gaps = Vec::new();
        for free in subtract(window, &self.busy(window)) {
            let mut known = Vec::new();
            let watched = timeline
                .observed
                .iter()
                .filter_map(|seen| clip(*seen, free));
            for seen in watched {
                known.extend(subtract(seen, &unsure));
            }
            for span in subtract(free, &known) {
                gaps.push(Gap {
                    span,
                    kind: GapKind::Unknown,
                });
            }
            gaps.extend(known.into_iter().map(|span| Gap {
                span,
                kind: GapKind::NoJob,
            }));
        }
        gaps.sort_by_key(|gap| gap.span.from);
        gaps
    }

    /// What its bars hold of the slot for sure: each run with both ends, and those still going on.
    fn busy(&self, window: Span) -> Vec<Span> {
        self.bars
            .iter()
            .filter_map(|bar| match (bar.started_at, bar.end) {
                (Some(from), BarEnd::At { at }) => Some(Span { from, to: at }),
                (Some(from), BarEnd::Open) => Some(Span {
                    from,
                    to: window.to,
                }),
                _ => None,
            })
            .collect()
    }
}

/// `span` within `within`; `None` where they do not overlap.
fn clip(span: Span, within: Span) -> Option<Span> {
    let from = span.from.max(within.from);
    let to = span.to.min(within.to);
    (from < to).then_some(Span { from, to })
}

/// What of `span` none of `taken` covers, oldest first.
fn subtract(span: Span, taken: &[Span]) -> Vec<Span> {
    let mut taken: Vec<Span> = taken.iter().filter_map(|part| clip(*part, span)).collect();
    taken.sort_by_key(|part| part.from);
    let mut left = Vec::new();
    let mut from = span.from;
    for part in taken {
        if part.from > from {
            left.push(Span {
                from,
                to: part.from,
            });
        }
        from = from.max(part.to);
    }
    if from < span.to {
        left.push(Span { from, to: span.to });
    }
    left
}

/// The lane of `key`, made empty the first time.
fn lane(lanes: &mut BTreeMap<SlotKey, Lane>, key: SlotKey) -> &mut Lane {
    lanes.entry(key).or_insert_with_key(|key| Lane {
        profile: key.profile.clone(),
        slot: key.slot,
        bars: Vec::new(),
        runners: Vec::new(),
    })
}

// The tests read the wire format, JSON, which only the host and client features read.
#[cfg(test)]
#[cfg(any(feature = "host", feature = "client"))]
mod tests;
