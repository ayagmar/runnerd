//! Insights as clients read them: what got slower, what is flaky, what nears its memory cap, what memory pressure
//! throttled and how long jobs wait, computed from the job history (ADR 0051); the daily digest; and what the
//! daemon's notifications carry. Times are seconds since the Unix epoch, durations seconds, sizes bytes.

use serde::{Deserialize, Serialize};

use super::RunnerFailure;
use super::jobs::JobSummary;

/// The window `GET /v1/insights` reads by default, in days.
pub const INSIGHT_DAYS: u32 = 14;

/// The longest window `GET /v1/insights` reads, in days.
pub const INSIGHT_DAYS_MAX: u32 = 90;

/// `GET /v1/insights` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsightsQuery {
    /// Days of history to read; default [`INSIGHT_DAYS`], at most [`INSIGHT_DAYS_MAX`].
    pub days: Option<u32>,
}

/// `GET /v1/insights`: what the history says about the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Insights {
    /// The start of the window.
    pub since: u64,
    /// When it was read: the end of the window.
    pub until: u64,
    /// Warnings first, then by repository, workflow and job.
    pub insights: Vec<Insight>,
    /// Each repository's waits this week and the week before, by repository. Always the last 14 days, whatever the
    /// window.
    pub waits: Vec<RepositoryWaits>,
}

/// How much an insight matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    /// Worth acting on: notifications send these.
    Warning,
    /// Worth knowing.
    Notice,
}

/// One thing the history says, about one job or one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Insight {
    /// The same while it holds, such as `slower:halyard/api/CI/test`.
    pub key: String,
    /// How much it matters.
    pub severity: Severity,
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name; `None` for a repository's waits, or when unknown.
    pub workflow: Option<String>,
    /// The job's name; `None` for a repository's waits.
    pub job: Option<String>,
    /// What it is, in one line.
    pub sentence: String,
    /// What it is, with its figures.
    #[serde(flatten)]
    pub kind: InsightKind,
    /// The runs that show it, newest first, at most 5.
    pub runs: Vec<RunLink>,
}

/// What an insight is, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum InsightKind {
    /// A job's successful runs on its busiest branch take longer than they did, and stayed so.
    Slower {
        /// The branch.
        branch: Option<String>,
        /// The median run time before, in seconds.
        before: u64,
        /// The median run time of the last runs, in seconds.
        now: u64,
        /// The first slow run after which no two runs in a row were fast.
        since: Option<RunLink>,
    },
    /// A job failed and then passed on a re-run of the same run.
    Flaky {
        /// Runs in which it did.
        flaky: u32,
        /// The job's runs in the window.
        total: u32,
    },
    /// A job's memory peaks near its profile's cap.
    NearCap {
        /// The profile.
        profile: String,
        /// The 95th percentile of its last runs' memory peaks.
        peak: u64,
        /// The profile's `memory`.
        cap: u64,
        /// What auto sizing does about it.
        auto: AutoNote,
    },
    /// A job's runs that write to the build cache find almost nothing in it: its cache keys differ from run to run.
    ColdCache {
        /// The branch those runs are on.
        branch: Option<String>,
        /// Lookups that found an entry, over those runs.
        hits: u64,
        /// Lookups over those runs.
        lookups: u64,
    },
    /// The kernel throttled a repository's jobs' downloads for memory pressure (`JobMetrics::sock_throttled`):
    /// the host, or a job at its cap, was short of memory.
    Throttled {
        /// Runs it throttled.
        throttled: u32,
        /// The repository's runs in the window.
        total: u32,
    },
    /// A repository's jobs wait longer this week than the week before.
    Waits {
        /// The 95th percentile of waits the week before, in seconds.
        before: u64,
        /// The 95th percentile of waits this week, in seconds.
        now: u64,
    },
}

/// What auto sizing does about a job near its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutoNote {
    /// It raises the profile's `memory` when a run meets the cap.
    Raises,
    /// The profile is at its `max_memory`: it cannot.
    AtMax,
    /// It is off.
    Off,
}

/// One run of a job, to open in the history or on GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLink {
    /// GitHub's job ID, the history's too.
    pub job: u64,
    /// The workflow run.
    pub run_id: u64,
    /// The run's attempt.
    pub run_attempt: u32,
    /// The branch.
    pub branch: Option<String>,
    /// The job's page on GitHub.
    pub url: Option<String>,
    /// When it finished.
    pub at: u64,
}

/// A repository's waits this week against the week before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryWaits {
    /// `owner/name`.
    pub repository: String,
    /// The jobs that started in the last 7 days.
    pub now: WeekWaits,
    /// The jobs that started in the 7 days before.
    pub before: WeekWaits,
}

/// The waits of the jobs that started in a week, from queued to started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeekWaits {
    /// Every job.
    pub all: WaitSpread,
    /// The jobs that found a runner already started: a warm runner (ADR 0050).
    pub warm: WaitSpread,
    /// The jobs whose runner started after they were queued.
    pub cold: WaitSpread,
}

/// How long some jobs waited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitSpread {
    /// How many.
    pub jobs: u32,
    /// The median, in seconds; `None` without jobs.
    pub p50: Option<u64>,
    /// The 95th percentile by nearest rank, in seconds; `None` without jobs.
    pub p95: Option<u64>,
}

/// `GET /v1/digest`: the last 24 hours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    /// 24 hours before `until`.
    pub since: u64,
    /// When it was read.
    pub until: u64,
    /// Jobs that finished: completed, or lost with their runner.
    pub finished: u64,
    /// Of those, the ones that failed, timed out or were lost.
    pub failed: u64,
    /// The insights that hold now and did not 24 hours ago.
    pub new: Vec<Insight>,
    /// The three longest waits of the jobs that started, longest first.
    pub waits: Vec<LongWait>,
}

/// A job that waited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongWait {
    /// GitHub's job ID.
    pub job: u64,
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name.
    pub workflow: Option<String>,
    /// The job's name.
    pub name: String,
    /// Seconds from queued to started.
    pub waited: u64,
    /// The job's page on GitHub.
    pub url: Option<String>,
}

/// What a notification target is sent, and the events `[notify]` targets choose from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyEvent {
    /// A job failed or timed out.
    Failed,
    /// A job's runner vanished before GitHub said the job completed.
    Lost,
    /// A runner could not be registered or started, and its repository is held back.
    Runner,
    /// A new warning insight.
    Insights,
    /// The daily digest.
    Digest,
    /// `runnerctl notify test`; not one a target chooses.
    Test,
}

/// How urgent a notification is, as ntfy ranks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    /// ntfy's 2: no sound.
    Low,
    /// ntfy's 3.
    Default,
    /// ntfy's 4: a longer vibration.
    High,
}

/// What a webhook target is posted, as JSON; an ntfy target gets its title, message, priority and link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// What it is about.
    pub event: NotifyEvent,
    /// One short line.
    pub title: String,
    /// A few lines.
    pub message: String,
    /// What to open: a job's page on GitHub, or nothing.
    pub url: Option<String>,
    /// How urgent.
    pub priority: Priority,
    /// The jobs it is about: failed or lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<JobSummary>,
    /// The runners that could not start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<RunnerFailure>,
    /// The insights it is about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub insights: Vec<Insight>,
    /// The digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
}

/// `POST /v1/notify/test` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyTestQuery {
    /// Only the target with this name; default every one.
    pub target: Option<String>,
}

/// `POST /v1/notify/test`: what each target did with a test message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyTest {
    /// In the config's order.
    pub targets: Vec<Delivery>,
}

/// What one target did with a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// The target's name.
    pub target: String,
    /// Why it was not delivered: an HTTP status or a connection error, never the URL. `None` when it was.
    pub error: Option<String>,
}
