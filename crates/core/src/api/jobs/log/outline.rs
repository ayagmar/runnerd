//! A job's log outline: the groups and the error and warning marks [`Outliner`] finds as lines are stored, which
//! lines each step printed as far as that can be told, how much of the log is there, and what a failed job's log
//! says of its failure. The store keeps an [`Outliner`]'s state and changes beside the lines; [`LogOutline::assemble`]
//! reads them back with the job's stored steps, which stay the authority on a step's times and outcome.
//!
//! Which lines a step printed is best effort: the log does not say. A segment starts at the first outlined line, at
//! every group and at the runner's `Post job cleanup.` and `Cleaning up orphan processes` lines. It goes to the one
//! step, at or after the previous segment's, whose stored times hold both its first and its last line's times,
//! narrowed by an exact `Run …` title when several do. Otherwise no step gets it. Never by position.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{LineKind, StyledLine, classify, plain, time_of};
use crate::api::{Job, RecordNote, RecordState, Step, Tone};

/// Groups nested deeper than this are counted, not kept.
pub(crate) const DEPTH_MAX: usize = 8;

/// Groups a job's outline keeps; later ones are counted, not kept.
pub(crate) const GROUP_MAX: u64 = 1000;

/// Error and warning lines a job's outline marks; later ones are counted, not marked.
pub(crate) const MARK_MAX: u64 = 1000;

/// Lines that start a step without a group (`Post job cleanup.`) an outline keeps.
const SECTION_MAX: u64 = 200;

/// A group's title is cut to this many bytes.
const TITLE_MAX: usize = 200;

/// How far back from a failed step's last line its exit code is looked for.
const EXIT_SCAN: u64 = 200;

/// The last lines of a failed step a failure summary shows.
const EXCERPT: u64 = 10;

/// Lines the runner prints when a step begins without a group: each post step, and `Complete job`.
const SECTIONS: [&str; 2] = ["Post job cleanup.", "Cleaning up orphan processes"];

/// How the runner says a script step failed: `##[error]Process completed with exit code 101.`.
const EXIT: &str = "Process completed with exit code ";

/// What the store keeps of an outline between batches of lines: one row per job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedOutline {
    /// The first line outlined: 0, or the next line of a job whose log an older runnerd began.
    pub from: u64,
    /// Error lines seen.
    pub errors: u64,
    /// Warning lines seen.
    pub warnings: u64,
    /// Groups kept.
    pub groups: u64,
    /// Error and warning marks kept.
    pub marks: u64,
    /// Section lines kept.
    pub sections: u64,
    /// Groups not kept: nested past 8 deep or past 1000 a job.
    pub dropped: u64,
    /// Of those, the ones still open: an `##[endgroup]` closes one of them before a kept one.
    pub hidden: u64,
    /// The kept groups still open, outermost first, by the line they start at.
    pub open: Vec<u64>,
}

/// What a line changes in the outline, for the store to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutlineChange {
    /// A group starts at `start`, inside `depth` open ones.
    Opened {
        /// Its `##[group]` line.
        start: u64,
        /// How many groups it is inside.
        depth: u32,
        /// Its title, plain and cut to 200 bytes.
        title: String,
    },
    /// The group that started at `start` ends at `end`, its `##[endgroup]` line.
    Closed {
        /// Its `##[group]` line.
        start: u64,
        /// Its `##[endgroup]` line.
        end: u64,
    },
    /// An error or warning line.
    Marked(LogMark),
    /// A line that starts a step without a group.
    Section {
        /// The line.
        line: u64,
    },
}

/// Builds a job's outline a line at a time. The store resumes one from its saved state for each batch of lines and
/// saves it after, in the transaction that stores the lines.
#[derive(Debug)]
pub struct Outliner {
    saved: SavedOutline,
}

impl Outliner {
    /// An outline starting at line `from`.
    pub fn new(from: u64) -> Outliner {
        Outliner {
            saved: SavedOutline {
                from,
                ..SavedOutline::default()
            },
        }
    }

    /// The outline the store saved.
    pub fn resume(saved: SavedOutline) -> Outliner {
        Outliner { saved }
    }

    /// What to save.
    pub fn saved(&self) -> &SavedOutline {
        &self.saved
    }

    /// Reads line `line`, with stored text `text`: what it changes, if anything.
    pub fn line(&mut self, line: u64, text: &str) -> Option<OutlineChange> {
        let saved = &mut self.saved;
        let (kind, rest) = classify(text);
        match kind {
            LineKind::Group => {
                if saved.hidden > 0 || saved.open.len() >= DEPTH_MAX || saved.groups >= GROUP_MAX {
                    saved.hidden += 1;
                    saved.dropped += 1;
                    return None;
                }
                let depth = u32::try_from(saved.open.len()).unwrap_or(u32::MAX);
                saved.open.push(line);
                saved.groups += 1;
                Some(OutlineChange::Opened {
                    start: line,
                    depth,
                    title: title(rest),
                })
            }
            LineKind::EndGroup => {
                if saved.hidden > 0 {
                    saved.hidden -= 1;
                    return None;
                }
                // One with no open group is left alone.
                let start = saved.open.pop()?;
                Some(OutlineChange::Closed { start, end: line })
            }
            LineKind::Error | LineKind::Warning => {
                let kind = if kind == LineKind::Error {
                    saved.errors += 1;
                    MarkKind::Error
                } else {
                    saved.warnings += 1;
                    MarkKind::Warning
                };
                (saved.marks < MARK_MAX).then(|| {
                    saved.marks += 1;
                    OutlineChange::Marked(LogMark { line, kind })
                })
            }
            LineKind::Output if SECTIONS.contains(&rest) && saved.sections < SECTION_MAX => {
                saved.sections += 1;
                Some(OutlineChange::Section { line })
            }
            _ => None,
        }
    }
}

/// A group's title: plain, cut to [`TITLE_MAX`] bytes.
fn title(text: &str) -> String {
    let mut title = plain(text);
    if title.len() > TITLE_MAX {
        let mut end = TITLE_MAX;
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        title.truncate(end);
    }
    title
}

/// Lines from `from` up to, not including, `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    /// The first line.
    pub from: u64,
    /// The line after the last.
    pub to: u64,
}

impl LineRange {
    /// Whether it holds no line.
    pub fn is_empty(&self) -> bool {
        self.to <= self.from
    }
}

/// A foldable group of lines, from its `##[group]` to its `##[endgroup]` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogGroup {
    /// Its `##[group]` line.
    pub start: u64,
    /// Its `##[endgroup]` line; absent while it is open, or when the log never closed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
    /// How many groups it is inside.
    pub depth: u32,
    /// Its title.
    pub title: String,
}

/// What a marked line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MarkKind {
    /// `##[error]`.
    Error,
    /// `##[warning]`.
    Warning,
    /// A kind a newer daemon names.
    #[serde(other)]
    Unknown,
}

/// An error or warning line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogMark {
    /// The line.
    pub line: u64,
    /// What it says.
    pub kind: MarkKind,
}

/// Which lines a step printed, as far as the log tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StepLog {
    /// These lines.
    Lines(LineRange),
    /// None: it was skipped or has not started.
    Empty,
    /// Its lines cannot be told from the others', or are not here: they may be among the unattributed ones.
    Unattributed,
    /// A reading a newer daemon gives; this client treats it as unattributed.
    #[serde(other)]
    Unknown,
}

/// A stored step and its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepLines {
    /// The step's number, as in `Job::steps`.
    pub number: u32,
    /// Its lines.
    pub lines: StepLog,
}

/// How much of a job's log there is to read, in order of precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogEvidence {
    /// Every line the job printed, outlined.
    Complete,
    /// The job still runs: lines may come.
    Running,
    /// Outlined only from `LogOutline::from`: an older runnerd stored the lines before it.
    Partial,
    /// Cut at `history.log_max`: what the job printed after was not kept.
    Truncated,
    /// Deleted after `history.logs`, or being deleted.
    Expired,
    /// No log was captured.
    Missing,
    /// Stored by a runnerd that kept no outline: the lines read, without their structure.
    NotOutlined,
    /// A reading a newer daemon gives.
    #[serde(other)]
    Unknown,
}

impl LogEvidence {
    /// What a reader needs told, if anything: `cut at history.log_max`.
    pub fn words(self) -> Option<&'static str> {
        match self {
            LogEvidence::Complete => None,
            LogEvidence::Running => Some("still running"),
            LogEvidence::Partial => Some("outlined from a later line"),
            LogEvidence::Truncated => Some("cut at history.log_max"),
            LogEvidence::Expired => Some("deleted after history.logs"),
            LogEvidence::Missing => Some("no log was captured"),
            LogEvidence::NotOutlined => Some("kept without an outline"),
            LogEvidence::Unknown => Some("read by a newer runnerd"),
        }
    }

    /// What it means: a cut log is worth a look.
    pub fn tone(self) -> Tone {
        match self {
            LogEvidence::Truncated => Tone::Warn,
            LogEvidence::Complete => Tone::Neutral,
            _ => Tone::Quiet,
        }
    }

    /// Whether lines past what is stored may still come.
    pub fn grows(self) -> bool {
        self == LogEvidence::Running
    }
}

/// What a failed job's log says of its failure. Each part is absent where the evidence is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureSummary {
    /// The first step GitHub reports failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    /// The exit code the runner printed in that step's lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// That step's last lines; absent when which lines are its is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<Vec<StyledLine>>,
    /// The command that opens a shell in the job's record, while it is kept: `runnerctl shell 42`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

impl FailureSummary {
    /// Its first line, for `steps`, the job's: `Step 4, Unit tests, failed with exit code 101`.
    pub fn headline(&self, steps: &[Step]) -> String {
        let Some(number) = self.step else {
            return "No step reports the failure".to_owned();
        };
        let mut text = match steps.iter().find(|step| step.number == number) {
            Some(step) => format!("Step {number}, {}, failed", step.name),
            None => format!("Step {number} failed"),
        };
        if let Some(code) = self.exit_code {
            write!(text, " with exit code {code}").ok();
        }
        text
    }
}

/// `GET /v1/jobs/{id}/log/outline`: a job's log as structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogOutline {
    /// The job.
    pub job: u64,
    /// How much of the log there is.
    pub evidence: LogEvidence,
    /// Lines stored: they are numbered from 0 up to this, less any deleted.
    pub lines: u64,
    /// The first line outlined; `lines` when none is.
    pub from: u64,
    /// Its groups, by their first line.
    pub groups: Vec<LogGroup>,
    /// Each stored step and its lines, in the job's order.
    pub steps: Vec<StepLines>,
    /// The stored lines no step got.
    pub unattributed: Vec<LineRange>,
    /// Error lines seen.
    pub errors: u64,
    /// Warning lines seen.
    pub warnings: u64,
    /// The error and warning lines, up to 1000.
    pub marks: Vec<LogMark>,
    /// Some groups or marks were only counted: more than it keeps.
    pub capped: bool,
    /// On a failed job: what its log says of the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureSummary>,
}

/// What the store has of a job's log besides its outline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoredLog {
    /// Lines stored, as `LogSummary::lines` counts them.
    pub lines: u64,
    /// Whether the log reached `history.log_max`.
    pub truncated: bool,
    /// Whether its first and its last line are still there: pruning deletes from the first.
    pub kept: bool,
}

/// What the store keeps of a job's outline: its saved state, `None` for a log no outline was kept of, and its groups,
/// marks and section lines, each by line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutlineParts {
    /// Its state.
    pub saved: Option<SavedOutline>,
    /// Its groups.
    pub groups: Vec<LogGroup>,
    /// Its marks.
    pub marks: Vec<LogMark>,
    /// The lines that start a step without a group.
    pub sections: Vec<u64>,
}

/// A segment of the log, from one possible step start to the next.
#[derive(Debug)]
struct Segment {
    range: LineRange,
    /// The times of its first and last lines, in seconds.
    first: Option<u64>,
    last: Option<u64>,
    /// The title of the group it starts with.
    title: Option<String>,
}

impl LogOutline {
    /// Assembles a job's outline from what the store keeps. `read(from, to)` returns the stored lines from `from` up
    /// to `to` with their numbers; it is asked for the first and last line of each segment, and for a failed step's
    /// last lines.
    pub fn assemble(
        job: &Job,
        stored: StoredLog,
        parts: OutlineParts,
        read: &mut dyn FnMut(u64, u64) -> Vec<(u64, String)>,
    ) -> LogOutline {
        let evidence = evidence(job, stored, parts.saved.as_ref());
        let outlined = matches!(
            evidence,
            LogEvidence::Complete
                | LogEvidence::Running
                | LogEvidence::Partial
                | LogEvidence::Truncated
        );
        let saved = parts.saved.unwrap_or_default();
        let from = if outlined { saved.from } else { stored.lines };
        let segments = if outlined {
            segments(from, stored.lines, &parts.groups, &parts.sections, read)
        } else {
            Vec::new()
        };
        let ranges = attribute(&job.steps, &segments);
        let steps: Vec<StepLines> = job
            .steps
            .iter()
            .zip(&ranges)
            .map(|(step, range)| StepLines {
                number: step.number,
                lines: match range {
                    Some(range) => StepLog::Lines(*range),
                    None if step.started_at.is_none()
                        || step.conclusion.as_deref() == Some("skipped") =>
                    {
                        StepLog::Empty
                    }
                    None => StepLog::Unattributed,
                },
            })
            .collect();
        let readable = !matches!(evidence, LogEvidence::Expired | LogEvidence::Missing);
        let unattributed = if readable {
            gaps(stored.lines, ranges.iter().flatten())
        } else {
            Vec::new()
        };
        let failure = job.summary.is_failure().then(|| failure(job, &steps, read));
        LogOutline {
            job: job.summary.id,
            evidence,
            lines: stored.lines,
            from,
            capped: saved.dropped > 0 || saved.marks < saved.errors + saved.warnings,
            errors: saved.errors,
            warnings: saved.warnings,
            groups: parts.groups,
            marks: parts.marks,
            steps,
            unattributed,
            failure,
        }
    }

    /// The outline of a whole log held in memory, as the store would keep it: for fixtures and the demo.
    pub fn of(job: &Job, lines: &[String]) -> LogOutline {
        let mut outliner = Outliner::new(0);
        let mut parts = OutlineParts::default();
        for (number, text) in (0..).zip(lines) {
            match outliner.line(number, text) {
                Some(OutlineChange::Opened {
                    start,
                    depth,
                    title,
                }) => {
                    parts.groups.push(LogGroup {
                        start,
                        end: None,
                        depth,
                        title,
                    });
                }
                Some(OutlineChange::Closed { start, end }) => {
                    if let Some(group) = parts.groups.iter_mut().find(|group| group.start == start)
                    {
                        group.end = Some(end);
                    }
                }
                Some(OutlineChange::Marked(mark)) => parts.marks.push(mark),
                Some(OutlineChange::Section { line }) => parts.sections.push(line),
                None => {}
            }
        }
        parts.saved = (!lines.is_empty()).then(|| outliner.saved().clone());
        let stored = StoredLog {
            lines: lines.len() as u64,
            truncated: job.summary.log.truncated,
            kept: true,
        };
        LogOutline::assemble(job, stored, parts, &mut |from, to| {
            (from..to)
                .filter_map(|number| {
                    let text = lines.get(usize::try_from(number).ok()?)?;
                    Some((number, text.clone()))
                })
                .collect()
        })
    }

    /// The lines of step `number`, when known.
    pub fn step_lines(&self, number: u32) -> Option<LineRange> {
        self.steps
            .iter()
            .find(|step| step.number == number)
            .and_then(|step| match step.lines {
                StepLog::Lines(range) => Some(range),
                _ => None,
            })
    }
}

fn evidence(job: &Job, stored: StoredLog, saved: Option<&SavedOutline>) -> LogEvidence {
    let fin = job.summary.state.is_final();
    if stored.lines == 0 {
        return if fin {
            LogEvidence::Missing
        } else {
            LogEvidence::Running
        };
    }
    if !stored.kept {
        return LogEvidence::Expired;
    }
    let Some(saved) = saved else {
        return LogEvidence::NotOutlined;
    };
    if stored.truncated {
        LogEvidence::Truncated
    } else if saved.from > 0 {
        LogEvidence::Partial
    } else if !fin {
        LogEvidence::Running
    } else {
        LogEvidence::Complete
    }
}

/// The log from `from` to `lines`, cut at every group and section line.
fn segments(
    from: u64,
    lines: u64,
    groups: &[LogGroup],
    sections: &[u64],
    read: &mut dyn FnMut(u64, u64) -> Vec<(u64, String)>,
) -> Vec<Segment> {
    let mut starts: Vec<u64> = std::iter::once(from)
        .chain(groups.iter().map(|group| group.start))
        .chain(sections.iter().copied())
        .filter(|start| (from..lines).contains(start))
        .collect();
    starts.sort_unstable();
    starts.dedup();
    let mut time = |line: u64| {
        read(line, line + 1)
            .into_iter()
            .next()
            .and_then(|(_, text)| time_of(&text))
    };
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(lines);
            Segment {
                range: LineRange {
                    from: start,
                    to: end,
                },
                first: time(start),
                last: time(end - 1),
                title: groups
                    .iter()
                    .find(|group| group.start == start)
                    .map(|group| group.title.clone()),
            }
        })
        .collect()
}

/// Whether `step` ran over `at`, in seconds: its stored times are whole seconds, so its last one is held to its end.
fn holds(step: &Step, at: Option<u64>) -> bool {
    let Some(started) = step.started_at else {
        return false;
    };
    at.is_none_or(|at| at >= started && step.completed_at.is_none_or(|end| at <= end))
}

/// Each step's lines, by index into `steps`: the segments it got, with any between them.
fn attribute(steps: &[Step], segments: &[Segment]) -> Vec<Option<LineRange>> {
    let mut ranges: Vec<Option<LineRange>> = vec![None; steps.len()];
    let mut earliest = 0;
    for segment in segments {
        let candidates: Vec<usize> = (earliest..steps.len())
            .filter(|&index| {
                let step = &steps[index];
                holds(step, segment.first) && holds(step, segment.last)
            })
            .collect();
        let chosen = match candidates.as_slice() {
            [only] => Some(*only),
            [] => None,
            several => {
                let titled: Vec<usize> = several
                    .iter()
                    .copied()
                    .filter(|&index| segment.title.as_deref() == Some(steps[index].name.as_str()))
                    .collect();
                match titled.as_slice() {
                    [only] => Some(*only),
                    _ => None,
                }
            }
        };
        if let Some(index) = chosen {
            earliest = index;
            let range = ranges[index].get_or_insert(segment.range);
            range.to = segment.range.to;
        }
    }
    ranges
}

/// The lines below `lines` that no range holds.
fn gaps<'range>(lines: u64, ranges: impl Iterator<Item = &'range LineRange>) -> Vec<LineRange> {
    let mut ranges: Vec<LineRange> = ranges.copied().collect();
    ranges.sort_unstable_by_key(|range| range.from);
    let mut gaps = Vec::new();
    let mut at = 0;
    for range in ranges {
        if range.from > at {
            gaps.push(LineRange {
                from: at,
                to: range.from,
            });
        }
        at = at.max(range.to);
    }
    if at < lines {
        gaps.push(LineRange {
            from: at,
            to: lines,
        });
    }
    gaps
}

/// What a failed job's log says of its failure.
fn failure(
    job: &Job,
    steps: &[StepLines],
    read: &mut dyn FnMut(u64, u64) -> Vec<(u64, String)>,
) -> FailureSummary {
    let step = job
        .steps
        .iter()
        .find(|step| step.conclusion.as_deref() == Some("failure"))
        .map(|step| step.number);
    let range = step.and_then(|number| {
        steps
            .iter()
            .find(|step| step.number == number)
            .and_then(|step| match step.lines {
                StepLog::Lines(range) => Some(range),
                _ => None,
            })
    });
    let tail: Vec<StyledLine> = range
        .map(|range| {
            read(range.to.saturating_sub(EXIT_SCAN).max(range.from), range.to)
                .into_iter()
                .map(|(number, text)| StyledLine::parse(number, &text))
                .collect()
        })
        .unwrap_or_default();
    let exit_code = tail.iter().rev().find_map(|line| {
        (line.kind == LineKind::Error)
            .then(|| line.text())
            .and_then(|text| {
                text.strip_prefix(EXIT)?
                    .strip_suffix('.')?
                    .parse::<i64>()
                    .ok()
            })
    });
    let excerpt = range.map(|_| {
        let skip = tail
            .len()
            .saturating_sub(usize::try_from(EXCERPT).unwrap_or(usize::MAX));
        tail[skip..].to_vec()
    });
    let shell = job
        .record
        .as_ref()
        .filter(|record| matches!(record.state(), RecordState::Kept { .. }))
        .map(|_| RecordNote::shell_command(job.summary.id));
    FailureSummary {
        step,
        exit_code,
        excerpt,
        shell,
    }
}
