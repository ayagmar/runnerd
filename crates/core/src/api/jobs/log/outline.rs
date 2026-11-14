//! A job's log outline: the groups and the error and warning marks [`Outliner`] finds as lines are stored, which
//! lines each step printed as far as that can be told, how much of the log is there, and what a failed job's log
//! says of its failure. The store keeps an [`Outliner`]'s state and changes beside the lines; [`LogOutline::assemble`]
//! reads them back with the job's stored steps, which stay the authority on a step's times and outcome.
//!
//! Which lines a step printed is best effort: the log does not say. A segment starts at the first outlined line, at
//! every group and at the lines the runner opens a step with but no group (`Post job cleanup.`, `Cleaning up orphan
//! processes`, a job hook's). GitHub stores a step's times in whole seconds, so time alone cannot split the steps
//! that share one, such as a quick failure and the skipped steps after it. The runner opens every step but `Set up
//! job` and the few it adds without a header (`HEADERLESS`, `HEADERLESS_GROUPS`) with a top-level `Run …` group
//! or one of those lines, so the log is cut there into blocks, and each step that ran printed one or more blocks, in
//! order. A block goes to a step when every way of giving each block to a step in order, each step its times hold and
//! each step that ran at least one block, gives it that step: a user group titled `Run …` adds a block no order
//! settles, and it goes to no step. Where a headerless step ran, the outline was capped, or no such way exists, a
//! segment goes to the one step, at or after the previous segment's, whose stored times hold the times of
//! its first and its last line that has one. A block or segment no time places goes to no step, unless it lies
//! between two of one step. Never by position, and never by a title alone: a step can print any title.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{LineKind, LineRead, StoredLine, StyledLine, classify, plain, prefix, time_of};
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

/// A group's title is read from at most this many bytes of its line.
pub(crate) const TITLE_SOURCE_MAX: usize = 4 * TITLE_MAX;

/// How much of a line is read for its time: the runner's timestamp and the space after it.
const TIME_PREFIX: u64 = 64;

/// How much of a failed step's line is read for its exit code: the runner's line is short.
const EXIT_PREFIX: u64 = 256;

/// How much of each line of a failure's excerpt is shown, in bytes; the rest is counted as omitted.
const EXCERPT_LINE_BYTES: u64 = 4 << 10;

/// How far back from a failed step's last line its exit code is looked for.
const EXIT_SCAN: u64 = 200;

/// The last lines of a failed step a failure summary shows.
const EXCERPT: u64 = 10;

/// Lines the runner prints when a step begins without a group: each post step, `Complete job`, and the steps of a
/// self-hosted runner's job hooks, whose `Run …` group follows.
const SECTIONS: [&str; 4] = [
    "Post job cleanup.",
    "Cleaning up orphan processes",
    "A job started hook has been configured by the self-hosted runner administrator",
    "A job completed hook has been configured by the self-hosted runner administrator",
];

/// How the runner titles the top-level group that opens a script or action step: `Run cargo test`.
const HEADER: &str = "Run ";

/// The steps the runner opens with no header: their lines join the block before them, or they print none, so a log
/// with one is not cut into blocks.
const HEADERLESS: [&str; 4] = [
    "Initialize containers",
    "Stop containers",
    "Wait for all background steps",
    "Create custom image",
];

/// How the runner titles the groups of the steps it adds for a Docker action, `Pull …` and `Build …`, which have no
/// header: a log with one is not cut into blocks. Their step names are not matched, since a user's step can be named
/// `Build the app`.
const HEADERLESS_GROUPS: [&str; 2] = ["Pull down action image", "Build container for action use"];

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

/// A title, such as a group's or a step's name: plain, read from its first [`TITLE_SOURCE_MAX`] bytes and cut to
/// [`TITLE_MAX`].
pub(crate) fn title(text: &str) -> String {
    let title = plain(prefix(text, TITLE_SOURCE_MAX));
    prefix(&title, TITLE_MAX).to_owned()
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
            Some(step) => format!("Step {number}, {}, failed", title(&step.name)),
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
}

impl LogOutline {
    /// How many lines a part of the log holds, as a fold says it: `1 line`, `5 lines`.
    pub fn count_words(lines: usize) -> String {
        if lines == 1 {
            "1 line".to_owned()
        } else {
            format!("{lines} lines")
        }
    }

    /// What a client says when it outlines only the log's last `shown` lines, `first` lines in: `The last 5000 lines;
    /// the full log has the 1200 before them`. `None` when it shows them all.
    pub fn tail_words(first: u64, shown: usize) -> Option<String> {
        (first > 0)
            .then(|| format!("the last {shown} lines; the full log has the {first} before them"))
    }

    /// Assembles a job's outline from what the store keeps, all read at one time. `read` returns the stored lines
    /// a [`LineRead`] asks for: the start of the first and last line of each segment, and a failed step's last
    /// lines, the most of them cut to a few hundred bytes.
    pub fn assemble(
        job: &Job,
        stored: StoredLog,
        parts: OutlineParts,
        read: &mut dyn FnMut(LineRead) -> Vec<StoredLine>,
    ) -> LogOutline {
        let evidence = evidence(job, stored, parts.saved.as_ref());
        let outlined = stored.kept && parts.saved.is_some();
        let saved = parts.saved.unwrap_or_default();
        let from = if outlined { saved.from } else { stored.lines };
        let segments = if outlined {
            segments(from, stored.lines, &parts.groups, &parts.sections, read)
        } else {
            Vec::new()
        };
        let ends = Ends {
            leading: from > 0,
            trailing: stored.truncated || !job.summary.state.is_final(),
        };
        let ranges = block_starts(from, stored.lines, &parts.groups, &parts.sections, &saved)
            .and_then(|starts| by_blocks(&job.steps, &segments, &starts, ends))
            .unwrap_or_else(|| attribute(&job.steps, &segments));
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
        LogOutline::assemble(job, stored, parts, &mut |read| read.of(lines))
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
    if stored.lines > 0 && !stored.kept {
        return LogEvidence::Expired;
    }
    // A log can reach its cap before its first line is stored.
    if stored.truncated {
        return LogEvidence::Truncated;
    }
    if stored.lines == 0 {
        return if fin {
            LogEvidence::Missing
        } else {
            LogEvidence::Running
        };
    }
    let Some(saved) = saved else {
        return LogEvidence::NotOutlined;
    };
    if saved.from > 0 {
        LogEvidence::Partial
    } else if !fin {
        LogEvidence::Running
    } else {
        LogEvidence::Complete
    }
}

/// The stored lines in `range`, each cut to `prefix` bytes.
fn read_range(
    read: &mut dyn FnMut(LineRead) -> Vec<StoredLine>,
    range: LineRange,
    prefix: u64,
) -> Vec<StoredLine> {
    let mut lines = read(LineRead {
        from: range.from,
        limit: range.to.saturating_sub(range.from),
        prefix,
        bytes: u64::MAX,
    });
    lines.retain(|line| line.line < range.to);
    lines
}

/// The log from `from` to `lines`, cut at every group and section line.
fn segments(
    from: u64,
    lines: u64,
    groups: &[LogGroup],
    sections: &[u64],
    read: &mut dyn FnMut(LineRead) -> Vec<StoredLine>,
) -> Vec<Segment> {
    let mut starts: Vec<u64> = std::iter::once(from)
        .chain(groups.iter().map(|group| group.start))
        .chain(sections.iter().copied())
        .filter(|start| (from..lines).contains(start))
        .collect();
    starts.sort_unstable();
    starts.dedup();
    let mut time = |line: u64| {
        read_range(
            read,
            LineRange {
                from: line,
                to: line + 1,
            },
            TIME_PREFIX,
        )
        .first()
        .and_then(|line| time_of(&line.text))
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
            }
        })
        .collect()
}

/// Whether `step` ran over `at`, in seconds: its stored times are whole seconds, so its last one is held to its end.
fn holds(step: &Step, at: u64) -> bool {
    step.started_at
        .is_some_and(|started| at >= started && step.completed_at.is_none_or(|end| at <= end))
}

/// Each step's lines, by index into `steps`: the segments it got, with any between them.
fn attribute(steps: &[Step], segments: &[Segment]) -> Vec<Option<LineRange>> {
    let mut ranges: Vec<Option<LineRange>> = vec![None; steps.len()];
    let mut earliest = 0;
    for segment in segments {
        let times: Vec<u64> = [segment.first, segment.last]
            .into_iter()
            .flatten()
            .collect();
        if times.is_empty() {
            continue;
        }
        let mut candidates = (earliest..steps.len())
            .filter(|&index| times.iter().all(|&at| holds(&steps[index], at)));
        let chosen = match (candidates.next(), candidates.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        };
        if let Some(index) = chosen {
            earliest = index;
            let range = ranges[index].get_or_insert(segment.range);
            range.to = segment.range.to;
        }
    }
    ranges
}

/// Which ends of the log may hold no line of a step that ran: the start of a log outlined from a later line, and the
/// end of one cut at its cap or still growing.
#[derive(Debug, Clone, Copy)]
struct Ends {
    leading: bool,
    trailing: bool,
}

/// Where the log's blocks start: the first outlined line, each line the runner opens a step with but no group, and
/// each top-level `Run …` group but one that follows such a line. `None` when a step's start may be missing: the
/// outline only counted some groups or section lines, or a Docker action's step printed no header.
fn block_starts(
    from: u64,
    lines: u64,
    groups: &[LogGroup],
    sections: &[u64],
    saved: &SavedOutline,
) -> Option<Vec<u64>> {
    let headerless = groups.iter().any(|group| {
        group.depth == 0
            && HEADERLESS_GROUPS
                .iter()
                .any(|title| group.title.starts_with(title))
    });
    if saved.dropped > 0 || saved.sections >= SECTION_MAX || headerless {
        return None;
    }
    let headers = groups
        .iter()
        .filter(|group| group.depth == 0 && group.title.starts_with(HEADER))
        .map(|group| group.start)
        .filter(|&start| start == 0 || !sections.contains(&(start - 1)));
    let mut starts: Vec<u64> = std::iter::once(from)
        .chain(sections.iter().copied())
        .chain(headers)
        .filter(|start| (from..lines).contains(start))
        .collect();
    starts.sort_unstable();
    starts.dedup();
    Some(starts)
}

/// A block of the log: the segments from one block start to the next.
#[derive(Debug)]
struct Block {
    /// Its first line, and the line after the last segment with a time: lines after that no time places.
    range: LineRange,
    /// The earliest and latest time of its lines, when any has one.
    times: Option<(u64, u64)>,
}

/// Each step's lines, by index into `steps`, from the blocks only one order gives it; `None` when no order fits, or a
/// step prints no header, and the segments are attributed by time alone.
fn by_blocks(
    steps: &[Step],
    segments: &[Segment],
    starts: &[u64],
    ends: Ends,
) -> Option<Vec<Option<LineRange>>> {
    if steps
        .iter()
        .any(|step| HEADERLESS.contains(&step.name.as_str()))
    {
        return None;
    }
    let mut blocks: Vec<Block> = Vec::new();
    for segment in segments {
        let opens = starts.binary_search(&segment.range.from).is_ok();
        if opens || blocks.is_empty() {
            blocks.push(Block {
                range: LineRange {
                    from: segment.range.from,
                    to: segment.range.from,
                },
                times: None,
            });
        }
        let block = blocks.last_mut()?;
        for at in [segment.first, segment.last].into_iter().flatten() {
            let (low, high) = block.times.get_or_insert((at, at));
            *low = (*low).min(at);
            *high = (*high).max(at);
            block.range.to = segment.range.to;
        }
    }
    // The steps that printed lines, if they ran: a skipped step prints none.
    let ran: Vec<usize> = (0..steps.len())
        .filter(|&index| {
            steps[index].started_at.is_some()
                && steps[index].conclusion.as_deref() != Some("skipped")
        })
        .collect();
    let owners = Orders::new(&blocks, steps, &ran, ends)?.owners();
    let mut ranges: Vec<Option<LineRange>> = vec![None; steps.len()];
    for (block, owner) in blocks.iter().zip(owners) {
        if let (Some(index), Some(_), false) = (owner, block.times, block.range.is_empty()) {
            let range = ranges[index].get_or_insert(block.range);
            range.to = block.range.to;
        }
    }
    Some(ranges)
}

/// The ways of giving each block to a step in order: block `i` and step `k` index `blocks` and `ran`. A step that
/// ended in success or failure printed at least one block; any other, or one before the outlined lines or after the
/// stored ones, may have printed none here.
struct Orders<'log> {
    blocks: &'log [Block],
    steps: &'log [Step],
    ran: &'log [usize],
    ends: Ends,
    /// Whether blocks `..i` can go to steps `..k`, step `k` given none of them yet, by `i * (ran + 1) + k`.
    fresh: Vec<bool>,
    /// The same, block `i - 1` given to step `k`.
    open: Vec<bool>,
}

impl<'log> Orders<'log> {
    /// The ways, or `None` when there are none.
    fn new(
        blocks: &'log [Block],
        steps: &'log [Step],
        ran: &'log [usize],
        ends: Ends,
    ) -> Option<Orders<'log>> {
        let cells = (blocks.len() + 1) * (ran.len() + 1);
        let mut orders = Orders {
            blocks,
            steps,
            ran,
            ends,
            fresh: vec![false; cells],
            open: vec![false; cells],
        };
        orders.fresh[0] = true;
        for i in 0..=blocks.len() {
            for k in 0..ran.len() {
                let at = orders.cell(i, k);
                if orders.fresh[at] && orders.may_skip(i, k) {
                    let next = orders.cell(i, k + 1);
                    orders.fresh[next] = true;
                }
                if orders.open[at] {
                    let next = orders.cell(i, k + 1);
                    orders.fresh[next] = true;
                }
                if i < blocks.len() && (orders.fresh[at] || orders.open[at]) && orders.fits(i, k) {
                    let next = orders.cell(i + 1, k);
                    orders.open[next] = true;
                }
            }
        }
        let done = orders.cell(blocks.len(), ran.len());
        orders.fresh[done].then_some(orders)
    }

    fn cell(&self, i: usize, k: usize) -> usize {
        i * (self.ran.len() + 1) + k
    }

    /// Whether step `k` may have printed no block, when blocks `..i` went to the steps before it.
    fn may_skip(&self, i: usize, k: usize) -> bool {
        let ended = matches!(
            self.steps[self.ran[k]].conclusion.as_deref(),
            Some("success" | "failure")
        );
        !ended || (i == 0 && self.ends.leading) || (i == self.blocks.len() && self.ends.trailing)
    }

    /// Whether step `k`'s stored times hold block `i`'s; a block with no time fits any step.
    fn fits(&self, i: usize, k: usize) -> bool {
        let step = &self.steps[self.ran[k]];
        self.blocks[i]
            .times
            .is_none_or(|(low, high)| holds(step, low) && holds(step, high))
    }

    /// The step, by index into the job's steps, that every way gives each block, if one does.
    fn owners(&self) -> Vec<Option<usize>> {
        let (blocks, ran) = (self.blocks.len(), self.ran.len());
        // Whether blocks `i..` can go to steps `k..` from each state, as `fresh` and `open` are read forward.
        let mut fresh = vec![false; self.fresh.len()];
        let mut open = vec![false; self.open.len()];
        let done = self.cell(blocks, ran);
        fresh[done] = true;
        for i in (0..=blocks).rev() {
            for k in (0..ran).rev() {
                let at = self.cell(i, k);
                let next_step = fresh[self.cell(i, k + 1)];
                let takes = i < blocks && self.fits(i, k) && open[self.cell(i + 1, k)];
                fresh[at] = (self.may_skip(i, k) && next_step) || takes;
                open[at] = next_step || takes;
            }
        }
        (0..blocks)
            .map(|i| {
                let mut owners = (0..ran).filter(|&k| {
                    let at = self.cell(i, k);
                    (self.fresh[at] || self.open[at])
                        && self.fits(i, k)
                        && open[self.cell(i + 1, k)]
                });
                match (owners.next(), owners.next()) {
                    (Some(only), None) => Some(self.ran[only]),
                    _ => None,
                }
            })
            .collect()
    }
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
    read: &mut dyn FnMut(LineRead) -> Vec<StoredLine>,
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
    let last = |count: u64, range: LineRange| LineRange {
        from: range.to.saturating_sub(count).max(range.from),
        to: range.to,
    };
    let exit_code = range.and_then(|range| {
        read_range(read, last(EXIT_SCAN, range), EXIT_PREFIX)
            .iter()
            .rev()
            .map(StoredLine::styled)
            .find_map(|line| {
                // A line read only in part is no exit message: what was cut off may make it something else.
                (line.kind == LineKind::Error && line.omitted == 0)
                    .then(|| line.text())
                    .and_then(|text| {
                        text.strip_prefix(EXIT)?
                            .strip_suffix('.')?
                            .parse::<i64>()
                            .ok()
                    })
            })
    });
    let excerpt = range.map(|range| {
        read_range(read, last(EXCERPT, range), EXCERPT_LINE_BYTES)
            .iter()
            .map(StoredLine::styled)
            .collect()
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
