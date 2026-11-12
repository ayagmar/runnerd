//! `GET /v1/jobs/{id}/compare` (ADR 0135): a job against an earlier run of the same job, its baseline. Step times
//! from the stored steps, matched by name only, never by position; whole-job figures where both jobs are final and
//! have them; a step's memory and CPU only where both steps' samples cover it, marked sampled. What is not known is
//! `None` and worded as unknown, never 0.
//!
//! Pure: the store finds the baseline and reads each step's samples, and [`JobComparison::of`] lays them side by
//! side, as the demo's fake daemon and the fixtures do from jobs in memory.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::facts::{samples_between, usage_of};
use super::log::{TITLE_SOURCE_MAX, title};
use super::{Job, JobState, JobSummary, Sample, Step};
use crate::api::{SAMPLE_INTERVAL, Tone, cpu_time, duration, in_cores};
use crate::config::ByteSize;

/// `GET /v1/jobs/{id}/compare` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareQuery {
    /// The job to compare with; default the baseline [`baseline_of`](JobComparison::baseline_of) picks.
    pub with: Option<u64>,
}

/// What a job is compared with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Baseline {
    /// This run of the same job.
    Chosen {
        /// Its ID.
        job: u64,
        /// The branch it ran for.
        branch: Option<String>,
        /// When it was queued.
        queued_at: u64,
        /// Whether the query named it (`with`), rather than the daemon picking it.
        asked: bool,
    },
    /// None: the reason says why.
    Unavailable {
        /// Why.
        reason: Unavailable,
    },
    /// The job `with` named is not a run of the same job.
    Incompatible {
        /// That job.
        job: u64,
    },
    /// A newer daemon's answer this version does not know.
    #[serde(other)]
    Other,
}

impl Baseline {
    /// Why there is none, worded; `None` when there is one.
    pub fn words(&self) -> Option<String> {
        match self {
            Baseline::Chosen { .. } => None,
            Baseline::Unavailable { reason } => Some(reason.words().to_owned()),
            Baseline::Incompatible { job } => Some(format!(
                "job {job} is not a run of this job: another repository, workflow or name"
            )),
            Baseline::Other => Some("not compared".to_owned()),
        }
    }
}

/// Why a job has no baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unavailable {
    /// No earlier run of the same job on its branch succeeded here, as far as the history keeps.
    NoEarlierSuccess,
    /// Its workflow is unknown, so no run is known to be the same job.
    UnknownWorkflow,
    /// Its branch is unknown, so no run is known to be on its branch.
    UnknownBranch,
    /// A newer daemon's reason this version does not know.
    #[serde(other)]
    Other,
}

impl Unavailable {
    /// Its words.
    pub fn words(self) -> &'static str {
        match self {
            Unavailable::NoEarlierSuccess => {
                "no earlier run of this job on its branch succeeded here"
            }
            Unavailable::UnknownWorkflow => {
                "its workflow is unknown, so no earlier run is known to be the same job"
            }
            Unavailable::UnknownBranch => {
                "its branch is unknown, so no earlier run is known to be on it"
            }
            Unavailable::Other => "no baseline",
        }
    }
}

/// What a figure measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FigureKind {
    /// Seconds from queued to started.
    Wait,
    /// Seconds from started to finished.
    Run,
    /// The highest memory of its samples, in bytes.
    MemoryPeak,
    /// CPU time, in milliseconds.
    CpuTime,
    /// CPU time over wall time, in millicores.
    CpuAverage,
    /// Bytes received.
    Received,
    /// A step's seconds from started to completed.
    StepTime,
    /// The highest memory of a step's samples, in bytes.
    StepMemory,
    /// A step's samples' CPU use averaged, in millicores.
    StepCpu,
    /// A newer daemon's figure this version does not know.
    #[serde(other)]
    Other,
}

impl FigureKind {
    /// Its label: `Wait`, `Memory peak`, `Step CPU`.
    pub fn label(self) -> &'static str {
        match self {
            FigureKind::Wait => "Wait",
            FigureKind::Run => "Run time",
            FigureKind::MemoryPeak | FigureKind::StepMemory => "Memory peak",
            FigureKind::CpuTime => "CPU time",
            FigureKind::CpuAverage | FigureKind::StepCpu => "Average CPU",
            FigureKind::Received => "Received",
            FigureKind::StepTime => "Time",
            FigureKind::Other => "Figure",
        }
    }

    /// A value of it with its unit: `1m 5s`, `4.1G`, `1.5 cores`.
    pub fn words(self, value: u64) -> String {
        match self {
            FigureKind::Wait | FigureKind::Run | FigureKind::StepTime => duration(value),
            FigureKind::MemoryPeak | FigureKind::Received | FigureKind::StepMemory => {
                ByteSize(value).to_string()
            }
            FigureKind::CpuTime => cpu_time(value),
            FigureKind::CpuAverage | FigureKind::StepCpu => in_cores(value),
            FigureKind::Other => value.to_string(),
        }
    }

    /// Whether more of it costs more: time and memory do; a CPU average or bytes received only say how it ran.
    fn is_cost(self) -> bool {
        matches!(
            self,
            FigureKind::Wait
                | FigureKind::Run
                | FigureKind::MemoryPeak
                | FigureKind::CpuTime
                | FigureKind::StepTime
                | FigureKind::StepMemory
        )
    }

    /// The least change worth a tone, beside a tenth: 5 seconds for a duration.
    fn floor(self) -> u64 {
        match self {
            FigureKind::Wait | FigureKind::Run | FigureKind::StepTime => 5,
            _ => 0,
        }
    }
}

/// How a figure changed from the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// It grew.
    More,
    /// It shrank.
    Less,
    /// It is the same.
    Same,
    /// One side is unknown.
    Unknown,
}

/// One figure of this job beside the baseline's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FigureDelta {
    /// What it measures.
    pub figure: FigureKind,
    /// This job's; `None` where unknown.
    pub this: Option<u64>,
    /// The baseline's; `None` where unknown, or without a baseline.
    pub baseline: Option<u64>,
    /// Whether both come from samples taken every few seconds, so a short peak between two is missed.
    #[serde(default)]
    pub sampled: bool,
}

impl FigureDelta {
    fn new(figure: FigureKind, this: Option<u64>, baseline: Option<u64>) -> FigureDelta {
        FigureDelta {
            figure,
            this,
            baseline,
            sampled: false,
        }
    }

    /// This job's figure with its unit, or `unknown`.
    pub fn this_words(&self) -> String {
        self.this
            .map_or_else(|| "unknown".to_owned(), |value| self.figure.words(value))
    }

    /// The baseline's figure with its unit, or `unknown`.
    pub fn baseline_words(&self) -> String {
        self.baseline
            .map_or_else(|| "unknown".to_owned(), |value| self.figure.words(value))
    }

    /// Which way it went.
    pub fn change(&self) -> ChangeKind {
        match (self.this, self.baseline) {
            (Some(this), Some(baseline)) if this > baseline => ChangeKind::More,
            (Some(this), Some(baseline)) if this < baseline => ChangeKind::Less,
            (Some(_), Some(_)) => ChangeKind::Same,
            _ => ChangeKind::Unknown,
        }
    }

    /// The change in words: `+1m 5s (+12%)`, `-300M (-8%)`, `same`, or `unknown`.
    pub fn change_words(&self) -> String {
        let (Some(this), Some(baseline)) = (self.this, self.baseline) else {
            return "unknown".to_owned();
        };
        let (sign, by) = match self.change() {
            ChangeKind::More => ('+', this - baseline),
            ChangeKind::Less => ('-', baseline - this),
            ChangeKind::Same | ChangeKind::Unknown => return "same".to_owned(),
        };
        let words = format!("{sign}{}", self.figure.words(by));
        match u128::from(by)
            .saturating_mul(100)
            .checked_div(u128::from(baseline))
        {
            Some(percent) => format!("{words} ({sign}{percent}%)"),
            None => words,
        }
    }

    /// What the change means: for a cost, a tenth more and past its floor is worth a look, a tenth less is better;
    /// an unknown side is background.
    pub fn tone(&self) -> Tone {
        let (Some(this), Some(baseline)) = (self.this, self.baseline) else {
            return Tone::Quiet;
        };
        let by = this.abs_diff(baseline);
        let marked = by >= self.figure.floor() && u128::from(by) * 10 > u128::from(baseline);
        match self.change() {
            ChangeKind::More if marked && self.figure.is_cost() => Tone::Warn,
            ChangeKind::Less if marked && self.figure.is_cost() => Tone::Good,
            _ => Tone::Neutral,
        }
    }
}

/// How a step of one job was paired with the other's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepMatch {
    /// The one step of this name in each job.
    Matched,
    /// Only this job has a step of this name.
    OnlyThis,
    /// Only the baseline has a step of this name.
    OnlyBaseline,
    /// A job has several steps of this name, so none of them is paired.
    Ambiguous,
    /// A newer daemon's pairing this version does not know.
    #[serde(other)]
    Other,
}

impl StepMatch {
    /// What it says beside the step, `None` for a matched one: `only in this run`, `only in the baseline`, `name
    /// not unique`.
    pub fn words(self) -> Option<&'static str> {
        match self {
            StepMatch::Matched => None,
            StepMatch::OnlyThis => Some("only in this run"),
            StepMatch::OnlyBaseline => Some("only in the baseline"),
            StepMatch::Ambiguous => Some("name not unique, not compared"),
            StepMatch::Other => Some("not compared"),
        }
    }
}

/// One step beside the baseline's step of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepDelta {
    /// The step's name, without escapes or control characters, cut to 200 bytes.
    pub name: String,
    /// How it was paired.
    pub matched: StepMatch,
    /// This job's step number, if it has the step.
    pub this: Option<u32>,
    /// The baseline's step number, if it has the step.
    pub baseline: Option<u32>,
    /// Its time on each side.
    pub time: FigureDelta,
    /// Its memory peak on each side, sampled: only where both steps' samples cover them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<FigureDelta>,
    /// Its average CPU on each side, as `memory`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<FigureDelta>,
}

/// Samples further apart than this many sample intervals, or as far from a step's start or end, leave part of the
/// step unsampled: its peak may be in it.
pub(crate) const SAMPLE_GAP_INTERVALS: u64 = 3;

/// What a step's samples say, as the store reads them over its window: the samples after its start up to its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepUsage {
    /// The step's number.
    pub number: u32,
    /// When its first sample was taken.
    pub first: u64,
    /// When its last was.
    pub last: u64,
    /// The longest time between two of its samples, in seconds; 0 for one sample.
    pub widest_gap: u64,
    /// Their highest memory, in bytes.
    pub memory_peak: u64,
    /// Their CPU use averaged, in millicores.
    pub cpu_average: u64,
}

impl StepUsage {
    /// What `samples`, a job's runner's, say over each of `steps` with both times, as the store reads its samples:
    /// those after a step's start up to its end. For a history held in memory, such as the demo's.
    pub fn of(samples: &[Sample], steps: &[Step]) -> Vec<StepUsage> {
        steps
            .iter()
            .filter_map(|step| {
                let (from, to) = (step.started_at?, step.completed_at?);
                let inside = samples_between(samples, from, to);
                let usage = usage_of(&inside)?;
                let widest_gap = inside
                    .windows(2)
                    .map(|pair| pair[1].at.saturating_sub(pair[0].at))
                    .max()
                    .unwrap_or(0);
                Some(StepUsage {
                    number: step.number,
                    first: inside.first()?.at,
                    last: inside.last()?.at,
                    widest_gap,
                    memory_peak: usage.memory_peak,
                    cpu_average: usage.cpu_average,
                })
            })
            .collect()
    }
}

/// The baseline as the store found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineRead {
    /// A run of the same job, and its steps' samples.
    Found {
        /// The job's summary.
        job: Box<JobSummary>,
        /// Its steps, as far as a comparison reads them.
        steps: ComparedSteps,
        /// Its steps' samples.
        usage: Vec<StepUsage>,
        /// Whether the query named it.
        asked: bool,
    },
    /// None, for this reason.
    Unavailable(Unavailable),
    /// The job named is not a run of the same job.
    Incompatible(u64),
}

/// `GET /v1/jobs/{id}/compare`: a job beside its baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobComparison {
    /// The job.
    pub job: u64,
    /// What it is compared with.
    pub baseline: Baseline,
    /// Whole-job figures, each given where the job is final; the baseline's always is.
    pub figures: Vec<FigureDelta>,
    /// This job's steps in order, then the baseline's steps it does not have; none without a baseline.
    pub steps: Vec<StepDelta>,
    /// Whether a job had more steps, or more of their names, than a comparison reads, so `steps` leaves its last
    /// ones out ([`COMPARE_STEPS_MAX`], [`COMPARE_STEP_BYTES`]).
    #[serde(default, skip_serializing_if = "is_false")]
    pub steps_cut: bool,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !value
}

/// The most steps of each job a comparison reads, in order of their numbers.
pub const COMPARE_STEPS_MAX: usize = 500;

/// The most bytes of step names of each job a comparison reads, each name read from its first 800 bytes, the most a
/// title is read from; a first step is read whatever its name's length.
pub const COMPARE_STEP_BYTES: usize = 128 * 1024;

/// A job's steps as far as a comparison reads them: the first [`COMPARE_STEPS_MAX`] in order, while their names,
/// each cut to its first 800 bytes, add up to at most [`COMPARE_STEP_BYTES`]. Steps pair by these names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComparedSteps {
    /// The steps read, in order.
    pub steps: Vec<Step>,
    /// Whether the job has steps past them.
    pub cut: bool,
}

impl ComparedSteps {
    /// `steps`, in order of their numbers, as the store reads them.
    pub fn of(steps: &[Step]) -> ComparedSteps {
        let mut ordered: Vec<&Step> = steps.iter().collect();
        ordered.sort_by_key(|step| step.number);
        let mut read = ComparedSteps::default();
        for step in ordered {
            let mut step = step.clone();
            let cut = step.name.floor_char_boundary(TITLE_SOURCE_MAX);
            step.name.truncate(cut);
            if !read.push(step) {
                read.cut = true;
                break;
            }
        }
        read
    }

    /// Adds `step`, its name already cut, unless the steps read are at a bound; whether it was added.
    pub fn push(&mut self, step: Step) -> bool {
        let bytes: usize = self.steps.iter().map(|step| step.name.len()).sum();
        let full = self.steps.len() >= COMPARE_STEPS_MAX
            || (!self.steps.is_empty() && bytes + step.name.len() > COMPARE_STEP_BYTES);
        if !full {
            self.steps.push(step);
        }
        !full
    }
}

/// A step's time in seconds, where both ends are known.
fn step_time(step: &Step) -> Option<u64> {
    Some(step.completed_at?.saturating_sub(step.started_at?))
}

/// Whether `usage` covers its step from start to end: its first sample near the step's start, its last near its
/// end, and none further apart than the samples' normal tolerance. A step whose samples were partly pruned, or
/// that ran while sampling stopped, is not covered: its peak may be in the part not sampled.
fn covers(usage: &StepUsage, step: &Step) -> bool {
    let tolerance = SAMPLE_INTERVAL * SAMPLE_GAP_INTERVALS;
    let (Some(from), Some(to)) = (step.started_at, step.completed_at) else {
        return false;
    };
    usage.first.saturating_sub(from) <= tolerance
        && to.saturating_sub(usage.last) <= tolerance
        && usage.widest_gap <= tolerance
}

impl JobComparison {
    /// Whether `other` is a run of the same job as `job`: the same repository, a known and equal workflow, the same
    /// name, and not `job` itself.
    pub fn same_job(job: &JobSummary, other: &JobSummary) -> bool {
        other.id != job.id
            && other.repository == job.repository
            && job.workflow.is_some()
            && other.workflow == job.workflow
            && other.name == job.name
    }

    /// Whether `other` may be `job`'s baseline: a run of the same job on the same known branch, queued before it
    /// (by time, then ID), that completed `success` with a runner here.
    pub fn is_baseline(job: &JobSummary, other: &JobSummary) -> bool {
        JobComparison::same_job(job, other)
            && job.branch.is_some()
            && other.branch == job.branch
            && other.state == JobState::Completed
            && other.conclusion.as_deref() == Some("success")
            && other.runner.is_some()
            && other.timeline.started_at.is_some()
            && (other.timeline.queued_at, other.id) < (job.timeline.queued_at, job.id)
    }

    /// `job`'s baseline among `history`: the latest that [`is_baseline`](JobComparison::is_baseline); or why there
    /// is none. As the store picks it.
    pub fn baseline_of<'history>(
        job: &JobSummary,
        history: &'history [JobSummary],
    ) -> Result<&'history JobSummary, Unavailable> {
        if job.workflow.is_none() {
            return Err(Unavailable::UnknownWorkflow);
        }
        if job.branch.is_none() {
            return Err(Unavailable::UnknownBranch);
        }
        history
            .iter()
            .filter(|other| JobComparison::is_baseline(job, other))
            .max_by_key(|other| (other.timeline.queued_at, other.id))
            .ok_or(Unavailable::NoEarlierSuccess)
    }

    /// Job `id` beside the job `with` names, or its baseline, in `history` held in memory, `usage` reading a job's
    /// steps' samples: as the store answers it, for the demo's fake daemon and the fixtures. `None` without either
    /// job. Steps and samples are read only once a baseline is found.
    pub fn in_history(
        id: u64,
        with: Option<u64>,
        history: &[Job],
        usage: impl Fn(&Job) -> Vec<StepUsage>,
    ) -> Option<JobComparison> {
        let find = |id: u64| history.iter().find(|job| job.summary.id == id);
        let this = find(id)?;
        let found = |other: &Job, asked: bool| BaselineRead::Found {
            job: Box::new(other.summary.clone()),
            steps: ComparedSteps::of(&other.steps),
            usage: usage(other),
            asked,
        };
        let baseline = if let Some(with) = with {
            let other = find(with)?;
            if JobComparison::same_job(&this.summary, &other.summary) {
                found(other, true)
            } else {
                BaselineRead::Incompatible(with)
            }
        } else {
            let summaries: Vec<JobSummary> =
                history.iter().map(|job| job.summary.clone()).collect();
            match JobComparison::baseline_of(&this.summary, &summaries) {
                Ok(other) => find(other.id).map_or(
                    BaselineRead::Unavailable(Unavailable::NoEarlierSuccess),
                    |other| found(other, false),
                ),
                Err(reason) => BaselineRead::Unavailable(reason),
            }
        };
        let (steps, usage) = match baseline {
            BaselineRead::Found { .. } => (ComparedSteps::of(&this.steps), usage(this)),
            _ => (ComparedSteps::default(), Vec::new()),
        };
        Some(JobComparison::of(&this.summary, &steps, &usage, baseline))
    }

    /// `job`, whose steps a comparison reads are `steps` and their samples `usage`, beside the baseline the store
    /// read. Without a baseline, `steps` and `usage` are not looked at, so the store need not read them.
    pub fn of(
        job: &JobSummary,
        steps: &ComparedSteps,
        usage: &[StepUsage],
        baseline: BaselineRead,
    ) -> JobComparison {
        let (other, other_steps, other_usage, asked) = match baseline {
            BaselineRead::Found {
                job,
                steps,
                usage,
                asked,
            } => (job, steps, usage, asked),
            BaselineRead::Unavailable(reason) => {
                return JobComparison::alone(job, Baseline::Unavailable { reason });
            }
            BaselineRead::Incompatible(other) => {
                return JobComparison::alone(job, Baseline::Incompatible { job: other });
            }
        };
        JobComparison {
            job: job.id,
            baseline: Baseline::Chosen {
                job: other.id,
                branch: other.branch.clone(),
                queued_at: other.timeline.queued_at,
                asked,
            },
            figures: figures(job, Some(&other)),
            steps: deltas(&steps.steps, usage, &other_steps.steps, &other_usage),
            steps_cut: steps.cut || other_steps.cut,
        }
    }

    /// `job` with no baseline: its own figures, each baseline side unknown, and no steps to pair.
    fn alone(job: &JobSummary, baseline: Baseline) -> JobComparison {
        JobComparison {
            job: job.id,
            baseline,
            figures: figures(job, None),
            steps: Vec::new(),
            steps_cut: false,
        }
    }

    /// What `steps` leaves out, worded; `None` when nothing.
    pub fn steps_cut_words(&self) -> Option<&'static str> {
        self.steps_cut.then_some(
            "some steps are not compared: a run has more steps, or longer step names, than a comparison reads",
        )
    }
}

/// The whole-job figures: each only for a final job, whose totals no longer change.
fn figures(job: &JobSummary, baseline: Option<&JobSummary>) -> Vec<FigureDelta> {
    let read = |job: &JobSummary, figure: FigureKind| -> Option<u64> {
        let timeline = &job.timeline;
        let metrics = &job.metrics;
        match figure {
            FigureKind::Wait => timeline
                .started_at
                .map(|started| started.saturating_sub(timeline.queued_at)),
            _ if !job.state.is_final() => None,
            FigureKind::Run => Some(timeline.finished_at?.saturating_sub(timeline.started_at?)),
            FigureKind::MemoryPeak => metrics.memory_peak,
            FigureKind::CpuTime => metrics.cpu_time,
            FigureKind::CpuAverage => metrics.cpu_average,
            FigureKind::Received => metrics.network_received,
            _ => None,
        }
    };
    [
        FigureKind::Wait,
        FigureKind::Run,
        FigureKind::MemoryPeak,
        FigureKind::CpuTime,
        FigureKind::CpuAverage,
        FigureKind::Received,
    ]
    .into_iter()
    .map(|figure| {
        FigureDelta::new(
            figure,
            read(job, figure),
            baseline.and_then(|baseline| read(baseline, figure)),
        )
    })
    .collect()
}

/// How many steps of each name a job has.
fn names(steps: &[Step]) -> BTreeMap<&str, usize> {
    let mut names = BTreeMap::new();
    for step in steps {
        *names.entry(step.name.as_str()).or_insert(0) += 1;
    }
    names
}

/// The steps side by side, paired by a name each job has once.
fn deltas(
    ours_steps: &[Step],
    usage: &[StepUsage],
    theirs_steps: &[Step],
    other_usage: &[StepUsage],
) -> Vec<StepDelta> {
    let ours = names(ours_steps);
    let theirs = names(theirs_steps);
    let unique = |name: &str| {
        ours.get(name).copied().unwrap_or(0) <= 1 && theirs.get(name).copied().unwrap_or(0) <= 1
    };
    let usage_of = |usage: &[StepUsage], step: &Step| {
        usage
            .iter()
            .find(|usage| usage.number == step.number)
            .filter(|usage| covers(usage, step))
            .copied()
    };
    let sampled = |figure: FigureKind, this: u64, other: u64| FigureDelta {
        figure,
        this: Some(this),
        baseline: Some(other),
        sampled: true,
    };
    let mut deltas = Vec::new();
    for step in ours_steps {
        let paired = unique(&step.name)
            .then(|| theirs_steps.iter().find(|theirs| theirs.name == step.name))
            .flatten();
        let matched = match (paired, unique(&step.name)) {
            (Some(_), _) => StepMatch::Matched,
            (None, false) => StepMatch::Ambiguous,
            (None, true) => StepMatch::OnlyThis,
        };
        let ours_used = usage_of(usage, step);
        let theirs_used = paired.and_then(|theirs| usage_of(other_usage, theirs));
        let both = ours_used.zip(theirs_used);
        deltas.push(StepDelta {
            name: title(&step.name),
            matched,
            this: Some(step.number),
            baseline: paired.map(|theirs| theirs.number),
            time: FigureDelta::new(
                FigureKind::StepTime,
                step_time(step),
                paired.and_then(step_time),
            ),
            memory: both.map(|(ours, theirs)| {
                sampled(FigureKind::StepMemory, ours.memory_peak, theirs.memory_peak)
            }),
            cpu: both.map(|(ours, theirs)| {
                sampled(FigureKind::StepCpu, ours.cpu_average, theirs.cpu_average)
            }),
        });
    }
    for theirs in theirs_steps {
        let unpaired = !unique(&theirs.name) || !ours.contains_key(theirs.name.as_str());
        if !unpaired {
            continue;
        }
        deltas.push(StepDelta {
            name: title(&theirs.name),
            matched: if unique(&theirs.name) {
                StepMatch::OnlyBaseline
            } else {
                StepMatch::Ambiguous
            },
            this: None,
            baseline: Some(theirs.number),
            time: FigureDelta::new(FigureKind::StepTime, None, step_time(theirs)),
            memory: None,
            cpu: None,
        });
    }
    deltas
}

// The tests read the wire format, JSON, which only the host and client features read.
#[cfg(test)]
#[cfg(any(feature = "host", feature = "client"))]
mod tests;
