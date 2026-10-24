//! The daemon's JSON API, version 1: the request and response types every client deserializes, and the
//! principal a transport authenticates. Plain data only: no I/O, no Docker or GitHub types, no secrets but a
//! [`Pairing`]'s one-time link, which its `Debug` leaves out.
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
//! | `POST /v1/dashboard/sign-out` | none, 204 |
//! | `POST /v1/dashboard/pairings?`[`PairingQuery`] | [`Pairings`]; 409 when the dashboard is off or has no `[web] origins` |
//! | `GET /v1/jobs?`[`JobQuery`] | [`JobPage`] |
//! | `GET /v1/jobs/{id}` | [`Job`] |
//! | `GET /v1/jobs/{id}/samples` | [`SampleSeries`] |
//! | `GET /v1/jobs/{id}/logs?`[`LogQuery`] | `text/plain`, one line per log line |
//! | `GET /v1/host/samples?`[`HostSampleQuery`] | [`HostSamples`] |
//! | `GET /v1/stats?`[`StatsQuery`] | [`Stats`] |
//! | `GET /v1/sizing?`[`SizingQuery`] | [`Sizing`] |
//! | `GET /v1/caches` | [`Caches`] |
//! | `GET /v1/caches/history?`[`CacheHistoryQuery`] | [`CacheHistory`] |
//! | `GET /v1/events?`[`EventQuery`] | `text/event-stream` of [`EventEnvelope`] |
//! | `GET /v1/records` | [`Records`] |
//! | `DELETE /v1/records/{job}` | [`RemovedRecords`]; 409 while a shell is open in it |
//! | `DELETE /v1/records?`[`RemoveRecordsQuery`] | [`RemovedRecords`]; records with an open shell stay |
//! | `POST /v1/records/{job}/shell?`[`ShellQuery`] | `Upgrade:` [`SHELL_PROTOCOL`], 101, then the shell's terminal; 409 when the budget is full |
//!
//! Errors carry an [`ErrorResponse`].

use serde::{Deserialize, Serialize};

mod admission;
mod control;
mod events;
mod jobs;
mod records;
mod sizing;

pub use admission::{Admission, AdmissionPolicy, Basis, CpuAdmission, JobPeak, cores, cpus};
pub use control::{
    Busy, Check, CheckStatus, ConfigCheck, ConfigFile, Doctor, PauseQuery, Paused, ReplaceQuery,
    RunningJob,
};
pub use events::{Event, EventEnvelope, EventQuery};
pub use jobs::{
    Activity, ActivityQuery, Bucket, CacheHistory, CacheHistoryQuery, CacheKind, CacheSize,
    CacheVolume, Caches, HostSample, HostSampleQuery, HostSamples, Job, JobMetrics, JobPage,
    JobQuery, JobRunner, JobState, JobSummary, LogQuery, LogSummary, Sample, SampleSeries, Started,
    Stats, StatsQuery, StatsRow, Step, Timeline,
};
pub use records::{
    Record, RecordLimits, RecordNote, RecordRemoval, Records, RemoveRecordsQuery, RemovedRecords,
    SHELL_PROTOCOL, ShellQuery,
};
pub use sizing::{
    Advice, AutoPlan, JobSizing, Peaks, ProfileSizing, Resize, Sizing, SizingChange, SizingHold,
    SizingQuery, Spread,
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
    /// A repository whose last listing failed, as while GitHub rate-limits runnerd, or that is no longer polled
    /// because runnerd is shutting down, shows no jobs here, while the history keeps its jobs queued.
    #[serde(default)]
    pub queued: Vec<QueuedJob>,
}

/// The memory budget runners and the mirror share, in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// The systemd slice runner containers run in.
    pub slice: String,
    /// The slice's `MemoryMax`; `None` when it is unlimited.
    pub memory_max: Option<u64>,
    /// Memory the running containers reserve: the runners' caps, or under measured admission the larger of each
    /// runner's predicted peak and its last sampled memory, and the caps of the seed containers, shells and the
    /// mirror.
    pub memory_in_use: u64,
    /// What a new runner must fit in beside them.
    #[serde(default)]
    pub admission: AdmissionPolicy,
    /// Under measured admission, whether the slice is at the high-water mark, so no runner starts.
    #[serde(default)]
    pub held: bool,
    /// The mirror's part of `memory_in_use`: its cap while it is enabled or still running, else 0.
    #[serde(default)]
    pub mirror_memory: u64,
    /// The seed containers' part of `memory_in_use`, one per profile that reseeds, by profile name.
    #[serde(default)]
    pub seed_memory: Vec<SeedMemory>,
    /// The shells' part of `memory_in_use`, one per shell open in a record, by job.
    #[serde(default)]
    pub shell_memory: Vec<ShellMemory>,
    /// The CPU budget, under `[host] cpu_overcommit`. Absent without one, and from a daemon before CPU admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuBudget>,
}

/// The CPU budget runners share, in millicores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuBudget {
    /// The CPUs the slice may use: its `AllowedCPUs` or the host's, less under a lower `CPUQuota`.
    pub cpus: u64,
    /// Those times `cpu_overcommit`: what the runners' reservations must fit.
    pub budget: u64,
    /// What the runners reserve: each its predicted average CPU under measured admission, else its `cpus`.
    pub in_use: u64,
    /// Whether a queued job waits for CPU that memory and a slot have room for.
    pub held: bool,
}

impl CpuBudget {
    /// The budget's line as clients show it beside memory, such as `CPU 4.5 of 32 CPUs reserved` or, while a job
    /// waits for it, `CPU 30 of 32 CPUs reserved; a job waits for CPU`.
    pub fn line(&self) -> String {
        let mut line = format!(
            "CPU {} of {} CPUs reserved",
            cores(self.in_use),
            cores(self.budget)
        );
        if self.held {
            line.push_str("; a job waits for CPU");
        }
        line
    }
}

impl Budget {
    /// How runners are admitted, as clients add it to the budget's line: nothing under caps, else `by predicted
    /// peaks`, and why nothing starts while the slice is at the high-water mark.
    pub fn admission_note(&self) -> Option<&'static str> {
        match (self.admission, self.held) {
            (AdmissionPolicy::Caps, _) => None,
            (AdmissionPolicy::Measured, false) => Some("by predicted peaks"),
            (AdmissionPolicy::Measured, true) => Some(
                "by predicted peaks; the slice is past 90% of its MemoryMax, so no runner starts",
            ),
        }
    }
}

/// A shell container's memory cap: its record's profile's limit when the shell opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellMemory {
    /// The job whose record the shell runs.
    pub job: u64,
    /// Its cap in bytes.
    pub memory: u64,
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
    /// What it takes of the budget: its cap under caps; under measured admission the larger of its predicted peak
    /// and its last sampled memory. 0 from a daemon before measured admission.
    #[serde(default)]
    pub reserved: u64,
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
    /// Why no runner has it yet, as the last plan saw it. `None` before the daemon has planned with it, while it
    /// shuts down, and from a daemon before it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Waiting>,
}

/// Why a queued job has no runner of its own yet. Jobs are served oldest first; each takes the first of these that
/// holds for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Waiting {
    /// Its repository is paused.
    Paused,
    /// Runners of its repository ended without taking a job, so none starts before `until` (seconds since the Unix
    /// epoch).
    Backoff {
        /// When runners may start again.
        until: u64,
    },
    /// A runner started for it and waits for GitHub to hand it the job: a runner takes any job its labels match,
    /// so not necessarily this one.
    RunnerStarting,
    /// Slot 1 of its profile is becoming the profile's snapshot, so none of the profile's slots starts.
    Reseeding {
        /// The profile.
        profile: String,
    },
    /// Its profile's image is being pulled, so none of the profile's slots starts.
    PullingImage {
        /// The profile.
        profile: String,
    },
    /// Every slot of its profile holds a runner.
    NoSlot {
        /// The profile.
        profile: String,
    },
    /// What its runner would reserve does not fit the memory budget left. Under measured admission, `free` is 0
    /// while the slice is at the high-water mark (`Budget::held`).
    Memory {
        /// What its runner would reserve, in bytes.
        needs: u64,
        /// What the budget has left, in bytes.
        free: u64,
    },
    /// What its runner would reserve does not fit the CPU budget left, in millicores.
    Cpu {
        /// What its runner would reserve.
        needs: u64,
        /// What the CPU budget has left.
        free: u64,
    },
    /// An older job has waited long enough for CPU that no younger one takes CPU before it.
    BehindCpu {
        /// The older job's ID.
        job: u64,
    },
}

/// `POST /v1/dashboard/pairings`: which of `[web] origins` to make a link for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingQuery {
    /// One of `[web] origins`; each of them when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// `POST /v1/dashboard/pairings`: fresh one-time links that sign a device in to the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairings {
    /// One per origin asked for.
    pub links: Vec<Pairing>,
}

/// A link that signs one device in to the dashboard on `origin`, and nowhere else, within `lifetime`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    /// One of `[web] origins`.
    pub origin: String,
    /// The link with its one-time code: a credential, which `Debug` leaves out.
    pub url: String,
    /// Seconds it works for from when it was made.
    pub lifetime: u64,
}

impl std::fmt::Debug for Pairing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pairing")
            .field("origin", &self.origin)
            .field("lifetime", &self.lifetime)
            .finish_non_exhaustive()
    }
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
