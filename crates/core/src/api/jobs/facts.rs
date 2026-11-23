//! What every client shows of a job: its name, its outcome and its steps', how long it waited and ran, its phases
//! and milestones, its figures as samples come in, what it used over a step, and how it compares with the same
//! job's latest runs. Each returns meaning, a label, a figure or an [`OutcomeKind`]; the dashboard, the TUI and
//! `runnerctl` only draw it, each colouring a kind its own way. Nothing here formats for one client.

use std::borrow::Cow;
use std::fmt;

use super::{
    FailureCause, Job, JobState, JobSummary, RunnerProbe, Sample, SampleSeries, Step, Timeline,
};
use crate::api::{Machine, QueuedJob, Record, Tone, Toned};

/// The outcome of a job that failed across a sleep of its host (`JobSummary::slept`).
const HOST_SLEPT: &str = "host slept";

/// What every client writes under the outcome of a job that failed across a sleep of its host: what failed it, as
/// `failure_line` says it, ahead of anything its log shows, which the sleep caused too.
const SLEPT_LINE: &str =
    "the host slept while it ran, which ended its runner's session with GitHub";

/// What kind of outcome a job's or a step's is, for each client to colour as it colours that meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    /// It succeeded.
    Succeeded,
    /// It failed.
    Failed,
    /// It failed because the runner failed it, not its code: a job read as `runner fault` (`JobSummary::runner_fault`).
    /// Only a job's outcome has it.
    RunnerFault,
    /// It is under way.
    Running,
    /// It has not begun: waiting for a runner, or a step not reached yet.
    Waiting,
    /// It ended without succeeding or failing: cancelled, skipped, run elsewhere, or ended before any runner took it.
    Ended,
}

/// A job's or a step's outcome: its word and what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Such as `success`, `timed out` or `running`.
    pub label: Cow<'static, str>,
    /// What it means.
    pub kind: OutcomeKind,
}

impl Outcome {
    fn new(label: &'static str, kind: OutcomeKind) -> Outcome {
        Outcome {
            label: Cow::Borrowed(label),
            kind,
        }
    }

    /// A conclusion of GitHub's, its underscores as spaces: `timed_out` is `timed out`.
    fn concluded(conclusion: &str) -> Outcome {
        let kind = match conclusion {
            "success" => OutcomeKind::Succeeded,
            failed if failed_conclusion(failed) => OutcomeKind::Failed,
            _ => OutcomeKind::Ended,
        };
        Outcome {
            label: Cow::Owned(conclusion.replace('_', " ")),
            kind,
        }
    }
}

impl FailureCause {
    /// Its word, as a client with little room writes it: `action download`, `action resolution` or `checkout`.
    /// `None` for a cause this build does not know.
    pub fn label(self) -> Option<&'static str> {
        match self {
            FailureCause::ActionArchive => Some("action download"),
            FailureCause::ActionResolve => Some("action resolution"),
            FailureCause::Checkout => Some("checkout"),
            FailureCause::Unknown => None,
        }
    }

    /// What it means, in a line. `None` for a cause this build does not know.
    pub fn line(self) -> Option<&'static str> {
        match self {
            FailureCause::ActionArchive => {
                Some("the runner could not download or unpack an action while it set up the job")
            }
            FailureCause::ActionResolve => {
                Some("the runner could not resolve an action with GitHub while it set up the job")
            }
            FailureCause::Checkout => Some("actions/checkout could not fetch the repository"),
            FailureCause::Unknown => None,
        }
    }
}

/// The conclusions that count as a failure, as `GET /v1/stats` and the insights count them.
fn failed_conclusion(conclusion: &str) -> bool {
    matches!(conclusion, "failure" | "timed_out")
}

impl JobSummary {
    /// Whether it failed, as `GET /v1/stats` counts failures: it concluded `failure` or `timed_out`, or it lost its
    /// runner. A job another machine ran is not this daemon's to count.
    pub fn is_failure(&self) -> bool {
        match self.state {
            JobState::Lost => true,
            JobState::Completed => self.conclusion.as_deref().is_some_and(failed_conclusion),
            JobState::Queued | JobState::Running | JobState::Elsewhere => false,
        }
    }

    /// Its outcome: where it is while it can change, then how it ended. Its kind is [`OutcomeKind::Failed`] or
    /// [`OutcomeKind::RunnerFault`] exactly when it [failed](JobSummary::is_failure); a job that concluded failed and
    /// that the runner failed reads `runner fault` (`runner_fault`), and one that failed or was lost across a sleep of
    /// its host `host slept` (`slept`); each counts as failed all the same. A job that left the queue without
    /// starting, as one a newer push cancelled, did not run elsewhere: it reads `cancelled in queue`, or as it
    /// concluded.
    pub fn outcome(&self) -> Outcome {
        outcome_of(
            self.state,
            self.conclusion.as_deref(),
            self.timeline.started_at.is_some(),
            Ended {
                runner_fault: self.runner_fault,
                slept: self.slept,
                elsewhere_runner: self.elsewhere_runner.as_deref(),
            },
        )
    }

    /// What failed it apart from its code, as every client writes it under its outcome: that the host slept while it
    /// ran (`slept`), unless its log says the runner failed it, or else what its log says, such as `the runner could
    /// not download or unpack an action while it set up the job`. `None` for a job that did not fail, and for one
    /// neither says anything of (`failure_cause`).
    pub fn failure_line(&self) -> Option<&'static str> {
        if !self.is_failure() {
            return None;
        }
        if self.slept && !self.runner_fault {
            return Some(SLEPT_LINE);
        }
        self.failure_cause.and_then(FailureCause::line)
    }

    /// That runnerd re-ran it, as every client writes it under its outcome: `runnerd re-ran the run's failed jobs as
    /// attempt 2, since the runner failed this one`. `None` for a job runnerd did not re-run (`rerun_at`).
    pub fn rerun_line(&self) -> Option<String> {
        self.rerun_at.map(|_| {
            format!(
                "runnerd re-ran the run's failed jobs as attempt {}, since the runner failed this one",
                self.run_attempt.saturating_add(1)
            )
        })
    }

    /// What the probe after its runner exited found (`probe`, ADR 0155), as every client writes it under its outcome,
    /// with its tone: `a probe after the job found the runner's mounts failing`. For a job that failed, and for any
    /// job whose probe found a call failing; a failed job without a verdict reads `no probe verdict recorded`, never
    /// as clean. `None` for every other job, and for a verdict this build does not know.
    pub fn probe_line(&self) -> Option<Toned> {
        let found = |text: &str, tone| {
            Some(Toned {
                text: text.to_owned(),
                tone,
            })
        };
        match self.probe {
            None if self.is_failure() => found("no probe verdict recorded", Tone::Quiet),
            None | Some(RunnerProbe::Unknown) => None,
            Some(RunnerProbe::Clean | RunnerProbe::Broken) if !self.is_failure() => None,
            Some(RunnerProbe::Clean) => found(
                "the probe's sampled mount and /proc calls succeeded",
                Tone::Quiet,
            ),
            Some(RunnerProbe::Broken) => found("a probe after the job could not run", Tone::Quiet),
            Some(RunnerProbe::Mount) => found(
                "a probe after the job found the runner's mounts failing",
                Tone::Warn,
            ),
            Some(RunnerProbe::Procfs) => found(
                "a probe after the job found the runner's /proc failing to open",
                Tone::Warn,
            ),
            Some(RunnerProbe::MountAndProcfs) => found(
                "a probe after the job found the runner's mounts and /proc failing",
                Tone::Warn,
            ),
            Some(RunnerProbe::TimedOut) => found(
                "a probe after the job did not finish within 3 seconds",
                Tone::Warn,
            ),
        }
    }

    /// Seconds it waited for a runner: until it started, or until `now` while it may still start. `None` for a job
    /// that ended without starting here.
    pub fn waited(&self, now: u64) -> Option<u64> {
        let end = match self.timeline.started_at {
            Some(started) => started,
            None if !self.state.is_final() => now,
            None => return None,
        };
        Some(end.saturating_sub(self.timeline.queued_at))
    }

    /// What of its wait at `now` passed while no runnerd ran, to follow it: `8h 10m of it while runnerd was not
    /// running`, never more than the wait. `None` when none did.
    pub fn unseen_note(&self, now: u64) -> Option<String> {
        let unseen = self.waited(now).map_or(self.timeline.unseen, |waited| {
            waited.min(self.timeline.unseen)
        });
        (unseen > 0).then(|| {
            format!(
                "{} of it while runnerd was not running",
                crate::api::duration(unseen)
            )
        })
    }

    /// What held its CPU back by `now`: `held at its CPU cap in 18% of its busy time, waited for a CPU 31% of its
    /// run`. The cap's part is left out without a quota's figures or busy periods, the wait's without pressure stall
    /// information or a run time; `None` when both are. A share that is not 0 never reads as 0%.
    pub fn cpu_held(&self, now: u64) -> Option<String> {
        let metrics = &self.metrics;
        let share = |part: u64, whole: u64| match part.saturating_mul(100) / whole.max(1) {
            0 => "under 1%".to_owned(),
            percent => format!("{}%", percent.min(100)),
        };
        let cap = metrics
            .cpu_periods
            .zip(metrics.cpu_throttled)
            .filter(|(periods, _)| *periods > 0)
            .map(|(periods, throttled)| match throttled {
                0 => "never held at its CPU cap".to_owned(),
                throttled => format!(
                    "held at its CPU cap in {} of its busy time",
                    share(throttled, periods)
                ),
            });
        let waited = metrics
            .cpu_stalled
            .zip(self.ran(now).filter(|ran| *ran > 0))
            .map(|(stalled, ran)| match stalled {
                0 => "never waited for a CPU".to_owned(),
                stalled => format!(
                    "waited for a CPU {} of its run",
                    share(stalled, ran.saturating_mul(1000))
                ),
            });
        let parts: Vec<String> = cap.into_iter().chain(waited).collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// Seconds it ran: until it finished, or until `now` while it may still finish. `None` before it started, and
    /// for a final job without a finish time, which no client claims ran for as long as it has been since.
    pub fn ran(&self, now: u64) -> Option<u64> {
        let started = self.timeline.started_at?;
        let end = match self.timeline.finished_at {
            Some(finished) => finished,
            None if !self.state.is_final() => now,
            None => return None,
        };
        Some(end.saturating_sub(started))
    }

    /// Its phases in order, each from when it began to when it ended, `None` while it goes on. A phase without an
    /// end goes on while the job can change; a final job's ends when the job finished, or is left out when no time
    /// says how long it took. Cleaning up follows the finish of a job whose runner started here, until its cleanup.
    pub fn phases(&self) -> Vec<Phase> {
        let timeline = &self.timeline;
        let open = !self.state.is_final();
        // `Some(end)` for a phase to show, `None` for one to leave out.
        let until = |end: Option<u64>| match end {
            Some(end) => Some(Some(end)),
            None if open => Some(None),
            None => timeline.finished_at.map(Some),
        };
        let runner = timeline.runner_from();
        let mut phases = Vec::new();
        if let Some(to) = until(runner.or(timeline.started_at)) {
            phases.push(Phase {
                kind: PhaseKind::Waiting,
                from: timeline.queued_at,
                to,
            });
        }
        if let Some(from) = runner
            && let Some(to) = until(timeline.started_at)
        {
            phases.push(Phase {
                kind: PhaseKind::Starting,
                from,
                to,
            });
        }
        if let Some(from) = timeline.started_at
            && let Some(to) = timeline.finished_at.map(Some).or(open.then_some(None))
        {
            phases.push(Phase {
                kind: PhaseKind::Running,
                from,
                to,
            });
        }
        if let Some(from) = timeline.finished_at
            && runner.is_some()
        {
            phases.push(Phase {
                kind: PhaseKind::CleaningUp,
                from,
                to: timeline.cleaned_at,
            });
        }
        phases
    }

    /// Its name as every client writes it: `workflow / job`.
    pub fn title(&self) -> JobTitle<'_> {
        job_title(self.workflow.as_deref(), &self.name, Some(self.id))
    }
}

/// What befell a job apart from its conclusion, as its record keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ended<'record> {
    /// Its log says the runner failed it (`JobSummary::runner_fault`).
    pub(crate) runner_fault: bool,
    /// The host slept while it ran (`JobSummary::slept`).
    pub(crate) slept: bool,
    /// The runner that ran it elsewhere (`JobSummary::elsewhere_runner`).
    pub(crate) elsewhere_runner: Option<&'record str>,
}

/// [`JobSummary::outcome`] of a job in `state`, concluded `conclusion`, that `started` here or not. A failure the log
/// says the runner caused reads `runner fault`; one across a sleep of the host, `host slept`, as the sleep ended the
/// runner's session. One that ran elsewhere on a runner GitHub names reads which machine ran it: `ran on skiff`.
pub(crate) fn outcome_of(
    state: JobState,
    conclusion: Option<&str>,
    started: bool,
    ended: Ended<'_>,
) -> Outcome {
    let Ended {
        runner_fault,
        slept,
        elsewhere_runner,
    } = ended;
    if state == JobState::Elsewhere
        && let Some(runner) = elsewhere_runner.filter(|runner| !runner.is_empty())
    {
        return Outcome {
            label: Cow::Owned(Machine::ran_on(runner)),
            kind: OutcomeKind::Ended,
        };
    }
    match (state, conclusion) {
        (JobState::Queued, _) => Outcome::new("queued", OutcomeKind::Waiting),
        (JobState::Running, _) => Outcome::new("running", OutcomeKind::Running),
        (JobState::Lost, _) if slept => Outcome::new(HOST_SLEPT, OutcomeKind::Failed),
        (JobState::Lost, _) => Outcome::new("lost", OutcomeKind::Failed),
        (JobState::Elsewhere, Some("cancelled")) if !started => {
            Outcome::new("cancelled in queue", OutcomeKind::Ended)
        }
        (JobState::Elsewhere, Some(conclusion)) if !started => Outcome {
            kind: OutcomeKind::Ended,
            ..Outcome::concluded(conclusion)
        },
        (JobState::Elsewhere, _) => Outcome::new("elsewhere", OutcomeKind::Ended),
        (JobState::Completed, None) => Outcome::new("completed", OutcomeKind::Ended),
        (JobState::Completed, Some(conclusion))
            if runner_fault && failed_conclusion(conclusion) =>
        {
            Outcome::new("runner fault", OutcomeKind::RunnerFault)
        }
        (JobState::Completed, Some(conclusion)) if slept && failed_conclusion(conclusion) => {
            Outcome::new(HOST_SLEPT, OutcomeKind::Failed)
        }
        (JobState::Completed, Some(conclusion)) => Outcome::concluded(conclusion),
    }
}

/// A stretch of a job's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase {
    /// Which.
    pub kind: PhaseKind,
    /// When it began.
    pub from: u64,
    /// When it ended; `None` while it goes on.
    pub to: Option<u64>,
}

impl Phase {
    /// How long it took, or has taken by `now`.
    pub fn seconds(&self, now: u64) -> u64 {
        self.to.unwrap_or(now).saturating_sub(self.from)
    }
}

/// The phases of a job, in the order they happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    /// Queued, without a runner.
    Waiting,
    /// A runner took it and is starting.
    Starting,
    /// The runner runs it.
    Running,
    /// It finished; its container and registration are being removed.
    CleaningUp,
}

impl PhaseKind {
    /// Its name, as every client labels it.
    pub fn label(self) -> &'static str {
        match self {
            PhaseKind::Waiting => "Waiting for a runner",
            PhaseKind::Starting => "Runner starting",
            PhaseKind::Running => "Running",
            PhaseKind::CleaningUp => "Cleaning up",
        }
    }
}

/// A moment of a job's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Milestone {
    /// Such as `Container started`.
    pub label: &'static str,
    /// When.
    pub at: u64,
}

impl Timeline {
    /// The moments it has times for, earliest first: a warm runner's container started before the job was queued.
    /// When runnerd first listed it splits its wait in three (ADR 0092): before it, runnerd had not seen the job;
    /// from it to its container's start, runnerd had; then the runner booted. A listing never comes before the
    /// queueing, however GitHub's clock and this machine's differ.
    pub fn milestones(&self) -> Vec<Milestone> {
        let mut milestones: Vec<Milestone> = [
            ("Queued", Some(self.queued_at)),
            (
                "Seen by runnerd",
                self.seen_at.map(|seen| seen.max(self.queued_at)),
            ),
            ("Container started", self.container_started_at),
            ("Job started", self.started_at),
            ("Finished", self.finished_at),
            ("Cleaned up", self.cleaned_at),
        ]
        .into_iter()
        .filter_map(|(label, at)| Some(Milestone { label, at: at? }))
        .collect();
        milestones.sort_by_key(|milestone| milestone.at);
        milestones
    }
}

impl Step {
    /// Its outcome: how it concluded, else whether it runs or waits. A step that timed out failed, one that was
    /// cancelled or skipped did not.
    pub fn outcome(&self) -> Outcome {
        match (self.conclusion.as_deref(), self.status.as_str()) {
            (Some(conclusion), _) => Outcome::concluded(conclusion),
            (None, "in_progress") => Outcome::new("running", OutcomeKind::Running),
            (None, "completed") => Outcome::new("completed", OutcomeKind::Ended),
            (None, _) => Outcome::new("pending", OutcomeKind::Waiting),
        }
    }

    /// Whether it runs now.
    pub fn is_running(&self) -> bool {
        self.status == "in_progress" && self.conclusion.is_none()
    }

    /// Seconds it took, until `now` while it runs; `None` before it started and for a step that ended without a
    /// completion time.
    pub fn took(&self, now: u64) -> Option<u64> {
        let started = self.started_at?;
        let end = match self.completed_at {
            Some(completed) => completed,
            None if self.is_running() => now,
            None => return None,
        };
        Some(end.saturating_sub(started))
    }
}

/// A job's name as every client writes it, in parts for a page that sets the workflow apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTitle<'job> {
    /// The workflow's name, when known.
    pub workflow: Option<&'job str>,
    /// The job's name, or `job <id>` until the daemon knows it.
    pub name: Cow<'job, str>,
}

/// `workflow / job`, or the job's name alone; for a job whose name the daemon does not know yet, as for one its
/// runner took before the daemon saw it queued, `job <id>`, or `a job` without an ID.
pub fn job_title<'job>(
    workflow: Option<&'job str>,
    name: &'job str,
    id: Option<u64>,
) -> JobTitle<'job> {
    let name = match (name, id) {
        ("", Some(id)) => Cow::Owned(format!("job {id}")),
        ("", None) => Cow::Borrowed("a job"),
        (name, _) => Cow::Borrowed(name),
    };
    JobTitle { workflow, name }
}

impl fmt::Display for JobTitle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.workflow {
            Some(workflow) => write!(f, "{workflow} / {}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

impl QueuedJob {
    /// Its name as [`JobSummary::title`] writes a job's.
    pub fn title(&self) -> JobTitle<'_> {
        job_title(self.workflow.as_deref(), &self.name, Some(self.id))
    }
}

impl Record {
    /// Its job's name as [`JobSummary::title`] writes a job's.
    pub fn title(&self) -> JobTitle<'_> {
        job_title(
            self.workflow.as_deref(),
            self.name.as_deref().unwrap_or_default(),
            Some(self.job),
        )
    }
}

impl Job {
    /// Takes a sample of its container as it arrives: while the job runs, its peaks rise to it, high-water marks
    /// that deleting old samples does not lower. A final job's figures are the daemon's totals and stay.
    pub fn observe(&mut self, sample: &Sample) {
        let metrics = &mut self.summary.metrics;
        if !self.summary.state.is_final() {
            metrics.memory_peak = metrics.memory_peak.max(Some(sample.memory));
            metrics.cpu_peak = metrics.cpu_peak.max(Some(sample.cpu));
        }
    }

    /// Takes the job as read again. While it runs, its peaks keep the highest seen, since the store's lag the
    /// samples a client saw arrive; once it is final, the read is the daemon's totals.
    pub fn reread(&mut self, read: Job) {
        let seen = self.summary.metrics.clone();
        *self = read;
        if !self.summary.state.is_final() {
            let metrics = &mut self.summary.metrics;
            metrics.memory_peak = metrics.memory_peak.max(seen.memory_peak);
            metrics.cpu_peak = metrics.cpu_peak.max(seen.cpu_peak);
        }
    }

    /// How it compares with the same job's latest `runs`, oldest first as given: this job among them as it is now,
    /// not as the runs were read, since it may have started, finished or peaked higher since.
    pub fn compare(&self, runs: &[JobSummary], now: u64) -> RunComparison {
        let runs: Vec<JobSummary> = runs
            .iter()
            .map(|run| {
                if run.id == self.summary.id {
                    self.summary.clone()
                } else {
                    run.clone()
                }
            })
            .collect();
        // Another machine's runs are not this daemon's to compare.
        let finished: Vec<&JobSummary> = runs
            .iter()
            .filter(|run| {
                matches!(run.state, JobState::Completed | JobState::Lost)
                    && run.timeline.started_at.is_some()
            })
            .collect();
        // The lower of the middle two for an even count: a duration one of the runs took.
        let median = |mut values: Vec<u64>| {
            values.sort_unstable();
            values.get(values.len().saturating_sub(1) / 2).copied()
        };
        RunComparison {
            median_ran: median(finished.iter().filter_map(|run| run.ran(now)).collect()),
            median_waited: median(finished.iter().filter_map(|run| run.waited(now)).collect()),
            finished: finished.len(),
            failed: finished.iter().filter(|run| run.is_failure()).count(),
            largest_peak: runs.iter().filter_map(|run| run.metrics.memory_peak).max(),
            runs,
        }
    }
}

/// A job's latest runs and what they say together, from [`Job::compare`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunComparison {
    /// Oldest first, this job as it is now.
    pub runs: Vec<JobSummary>,
    /// The median of the finished runs' run times.
    pub median_ran: Option<u64>,
    /// The median of the finished runs' waits.
    pub median_waited: Option<u64>,
    /// The runs that started and ended here: those the medians and failures count.
    pub finished: usize,
    /// Of those, the ones that [failed](JobSummary::is_failure).
    pub failed: usize,
    /// The largest memory peak of any run.
    pub largest_peak: Option<u64>,
}

impl SampleSeries {
    /// What the container used from `from` to `to`, from the samples taken after `from` up to `to`: each covers the
    /// interval before it, so steps that meet at a second share no sample. `None` when none was taken in it.
    pub fn usage_between(&self, from: u64, to: u64) -> Option<SampleUsage> {
        usage_of(&samples_between(&self.samples, from, to))
    }
}

/// The samples of `samples` taken after `from` up to `to`.
pub(crate) fn samples_between(samples: &[Sample], from: u64, to: u64) -> Vec<&Sample> {
    samples
        .iter()
        .filter(|sample| from < sample.at && sample.at <= to)
        .collect()
}

/// What `inside` say together: their highest memory and their CPU averaged; `None` for no sample.
pub(crate) fn usage_of(inside: &[&Sample]) -> Option<SampleUsage> {
    let count = u64::try_from(inside.len())
        .ok()
        .filter(|count| *count > 0)?;
    Some(SampleUsage {
        memory_peak: inside.iter().map(|sample| sample.memory).max()?,
        cpu_average: inside.iter().map(|sample| sample.cpu).sum::<u64>() / count,
    })
}

/// What a container used over a stretch of its run, from [`SampleSeries::usage_between`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleUsage {
    /// The highest memory of the samples, without page cache.
    pub memory_peak: u64,
    /// The samples' CPU use averaged, in millicores.
    pub cpu_average: u64,
}

impl SampleUsage {
    /// `peak 4.1G`.
    pub fn memory(&self) -> String {
        format!("peak {}", crate::config::ByteSize(self.memory_peak))
    }

    /// `average 1.5 cores`.
    pub fn cpu(&self) -> String {
        format!("average {}", super::super::in_cores(self.cpu_average))
    }

    /// Both, `peak 4.1G, average 1.5 cores`.
    pub fn description(&self) -> String {
        format!("{}, {}", self.memory(), self.cpu())
    }
}

#[cfg(test)]
mod tests;
