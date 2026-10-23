//! The daemon's JSON API, version 1: the request and response types every client deserializes, and the
//! principal a transport authenticates. Plain data only: no I/O, no Docker or GitHub types, no secrets.
//!
//! Routes, all under the authenticated principal:
//!
//! | Route | Body |
//! |---|---|
//! | `GET /v1/status` | [`Status`] |
//! | `POST /v1/repositories/{owner}/{name}/pause?`[`PauseQuery`] | [`Paused`]; 409 [`Busy`] while it runs a job |
//! | `POST /v1/repositories/{owner}/{name}/resume` | none, 204 |
//! | `POST /v1/repositories/pause?`[`PauseQuery`] | [`Paused`]; 409 [`Busy`] while any runs a job, pausing none |
//! | `POST /v1/repositories/resume` | none, 204: every repository resumed |
//! | `POST /v1/profiles/{name}/reseed` | none, 202; 409 while it reseeds; `cache.reseeded` when done |
//! | `POST /v1/caches/{name}/clean` | none, 204; 409 [`Busy`] while a runner uses it |
//! | `GET /v1/config` | [`ConfigFile`] |
//! | `POST /v1/config/check` | TOML in, [`ConfigCheck`] |
//! | `PUT /v1/config` | TOML in, [`ConfigCheck`]; 422 [`ConfigCheck`] when invalid |
//! | `GET /v1/doctor` | [`Doctor`] |
//! | `POST /v1/shutdown` | [`ShutdownRequest`] in, 202 |
//! | `GET /v1/jobs?`[`JobQuery`] | [`JobPage`] |
//! | `GET /v1/jobs/{id}` | [`Job`] |
//! | `GET /v1/jobs/{id}/samples` | [`SampleSeries`] |
//! | `GET /v1/jobs/{id}/logs?`[`LogQuery`] | `text/plain`, one line per log line |
//! | `GET /v1/host/samples?`[`HostSampleQuery`] | [`HostSamples`] |
//! | `GET /v1/stats?`[`StatsQuery`] | [`Stats`] |
//! | `GET /v1/caches` | [`Caches`] |
//! | `GET /v1/caches/history?`[`CacheHistoryQuery`] | [`CacheHistory`] |
//! | `GET /v1/events?`[`EventQuery`] | `text/event-stream` of [`EventEnvelope`] |
//!
//! Errors carry an [`ErrorResponse`].

use serde::{Deserialize, Serialize};

mod control;
mod events;
mod jobs;

pub use control::{
    Busy, Check, CheckStatus, ConfigCheck, ConfigFile, Doctor, PauseQuery, Paused, RunningJob,
};
pub use events::{Event, EventEnvelope, EventQuery};
pub use jobs::{
    Bucket, CacheHistory, CacheHistoryQuery, CacheKind, CacheSize, CacheVolume, Caches, HostSample,
    HostSampleQuery, HostSamples, Job, JobMetrics, JobPage, JobQuery, JobRunner, JobState,
    JobSummary, LogQuery, LogSummary, Sample, SampleSeries, Stats, StatsQuery, StatsRow, Step,
    Timeline,
};

/// Who sent a request, as the transport authenticated it. Handlers get it from one place, so a new transport only
/// has to produce one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Principal {
    /// The user the daemon runs as, on the local Unix socket.
    LocalOwner {
        /// The Unix user ID the socket's peer runs as.
        uid: u32,
    },
}

/// `GET /v1/status`: what the daemon serves and runs right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The daemon's version, from its `Cargo.toml`.
    pub version: String,
    /// Set once the daemon stops polling to shut down.
    pub shutting_down: bool,
    /// The memory budget and what runners use of it.
    pub budget: Budget,
    /// Every configured profile with its slots.
    pub profiles: Vec<ProfileStatus>,
    /// Every configured repository.
    pub repositories: Vec<RepositoryStatus>,
    /// Queued jobs the repositories' runners can take and none holds yet, at the last poll, oldest first. No event
    /// carries the list: `job.queued` announces a job, `job.started` and `job.finished` its leaving the queue.
    #[serde(default)]
    pub queued: Vec<QueuedJob>,
}

/// The memory budget runners and the mirror share, in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// The systemd slice runner containers run in.
    pub slice: String,
    /// The slice's `MemoryMax`; `None` when it is unlimited.
    pub memory_max: Option<u64>,
    /// Memory caps of the running containers: runners, seed containers and the mirror.
    pub memory_in_use: u64,
    /// The mirror's part of `memory_in_use`: its cap while it is enabled or still running, else 0.
    #[serde(default)]
    pub mirror_memory: u64,
    /// The seed containers' part of `memory_in_use`, one per profile that reseeds, by profile name.
    #[serde(default)]
    pub seed_memory: Vec<SeedMemory>,
}

/// A running seed container's memory cap: its profile's limit when the reseed started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedMemory {
    /// The profile it reseeds.
    pub profile: String,
    /// Its cap in bytes.
    pub memory: u64,
}

/// A runner profile and what runs in its slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileStatus {
    /// The name under `[profiles]` in `runnerd.toml`.
    pub name: String,
    /// The image its runners run, such as `runnerd-runner:base`.
    pub image: String,
    /// CPUs per slot.
    pub cpus: u32,
    /// Bytes per slot.
    pub memory: u64,
    /// Set while slot 1's layers become the profile's snapshot.
    pub reseeding: bool,
    /// Index 0 is slot 1.
    pub slots: Vec<SlotStatus>,
}

/// One slot of a profile: room for one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotStatus {
    /// 1-based.
    pub slot: usize,
    /// `None` when the slot is free.
    pub runner: Option<RunnerStatus>,
}

/// A runner registered for one job, in its container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerStatus {
    /// `owner/name` the runner is registered on.
    pub repository: String,
    /// The runner's name, also its container's.
    pub name: String,
    /// The runner's ID on GitHub.
    pub runner_id: u64,
    /// The Docker container ID.
    pub container: String,
    /// Whether GitHub reports it running a job.
    pub busy: bool,
    /// Seconds since the Unix epoch.
    pub started_at: u64,
}

/// A configured repository and its queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryStatus {
    /// `owner/name` on GitHub.
    pub name: String,
    /// The profile its jobs run in.
    pub profile: String,
    /// The labels its runners register with.
    pub labels: Vec<String>,
    /// Paused repositories get no new runners; their running jobs finish.
    pub paused: bool,
    /// Queued jobs whose labels these runners match, at the last poll.
    pub queued_jobs: usize,
}

/// A queued job runners of this daemon can take, as GitHub listed it at the last poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedJob {
    /// GitHub's job ID.
    pub id: u64,
    /// The repository whose runners can take it, `owner/name`.
    pub repository: String,
    /// The workflow run it belongs to.
    pub run_id: u64,
    /// 1 for the first attempt of the run, higher for re-runs.
    pub run_attempt: u32,
    /// The workflow's name.
    pub workflow: Option<String>,
    /// The job's name in the workflow.
    pub name: String,
    /// The branch the run is for.
    pub branch: Option<String>,
    /// The job's page on GitHub.
    pub url: Option<String>,
    /// When GitHub queued it, in seconds since the Unix epoch; 0 if GitHub did not say.
    pub queued_at: u64,
}

/// `POST /v1/shutdown`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownRequest {
    /// Kill running jobs and delete their runners instead of waiting for them.
    #[serde(default)]
    pub force: bool,
}

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// What went wrong, for people to read.
    pub message: String,
}
