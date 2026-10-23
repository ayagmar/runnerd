//! Job history as clients read it: one record per GitHub job a runner of this daemon ran, its timeline, steps,
//! metrics and log size; the queries that list them; sample series for graphs; per-repository aggregates; cache
//! sizes. Times are seconds since the Unix epoch, sizes are bytes, CPU is in millicores (1000 = one core busy).

use serde::{Deserialize, Serialize};

/// Where a job is in its life, as this daemon saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobState {
    /// Waiting for a runner.
    Queued,
    /// A runner of this daemon took it.
    Running,
    /// GitHub reports it completed; `conclusion` says how.
    Completed,
    /// Its runner's container is gone and GitHub did not report the job completed in time.
    Lost,
    /// A runner of another machine took it, or it was cancelled before any runner did.
    Elsewhere,
}

impl JobState {
    /// Whether the job can still change.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Lost | JobState::Elsewhere
        )
    }
}

/// A job without its steps: what lists and events carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummary {
    /// GitHub's job ID, also its check run ID.
    pub id: u64,
    /// `owner/name` on GitHub.
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
    /// The workflow's `runs-on` labels.
    pub labels: Vec<String>,
    /// Where the job is.
    pub state: JobState,
    /// GitHub's conclusion once completed: `success`, `failure`, `cancelled`, `skipped`, `timed_out`, ….
    pub conclusion: Option<String>,
    /// The runner that ran it.
    pub runner: Option<JobRunner>,
    /// When each phase started.
    pub timeline: Timeline,
    /// CPU, memory and network use while it ran.
    pub metrics: JobMetrics,
    /// How much of its log is stored.
    pub log: LogSummary,
}

/// `GET /v1/jobs/{id}`: a job with its steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Everything but the steps.
    #[serde(flatten)]
    pub summary: JobSummary,
    /// The steps as GitHub numbers them, including `Set up job` and `Complete job`.
    pub steps: Vec<Step>,
    /// Its record, for a failed job of a profile that records failures.
    #[serde(default)]
    pub record: Option<super::RecordNote>,
    /// How its runner was admitted; `None` for one started before runnerd recorded it.
    #[serde(default)]
    pub admission: Option<super::Admission>,
}

/// The runner a job ran on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRunner {
    /// The runner's name, also its container's.
    pub name: String,
    /// The profile it ran in.
    pub profile: String,
    /// The slot it ran in, 1-based.
    pub slot: usize,
    /// Its container's memory cap in bytes, which a reload since may have changed in the profile. `None` in records
    /// from before runnerd kept it.
    pub memory_cap: Option<u64>,
    /// Its container's CPU limit in millicores, the unit of the CPU use in `Sample` and `JobMetrics`. `None` in
    /// records from before runnerd kept it, or when Docker reports no limit.
    pub cpu_cap: Option<u64>,
}

/// A job's phases, in the order they happen. A runner's container starts before GitHub gives it a job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeline {
    /// GitHub queued the job.
    pub queued_at: u64,
    /// The container of the runner that took it started.
    pub container_started_at: Option<u64>,
    /// The runner started the job.
    pub started_at: Option<u64>,
    /// The job finished, by GitHub's clock; or when its container was found gone.
    pub finished_at: Option<u64>,
    /// The container and the runner's registration were removed.
    pub cleaned_at: Option<u64>,
}

/// What a job used, over the samples taken while it ran (every few seconds). `None` until a sample exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobMetrics {
    /// CPU time, in milliseconds.
    pub cpu_time: Option<u64>,
    /// The highest CPU use of a sample interval, in millicores.
    pub cpu_peak: Option<u64>,
    /// CPU time over wall time, in millicores.
    pub cpu_average: Option<u64>,
    /// The highest memory use without page cache.
    pub memory_peak: Option<u64>,
    /// The average memory use without page cache.
    pub memory_average: Option<u64>,
    /// Bytes the container received.
    pub network_received: Option<u64>,
    /// Bytes the container sent.
    pub network_sent: Option<u64>,
    /// Of the bytes received, those the Docker Hub mirror served.
    pub mirror_bytes: u64,
}

/// A job's stored log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSummary {
    /// Lines stored.
    pub lines: u64,
    /// Bytes stored.
    pub bytes: u64,
    /// Set once the log reached `history.log_max` and later lines were dropped.
    pub truncated: bool,
}

/// One step of a job, as GitHub reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// GitHub's step number; numbers may skip.
    pub number: u32,
    /// The name GitHub shows, such as `Set up job` or `Run docker run --rm hello-world`.
    pub name: String,
    /// `pending`, `queued`, `in_progress` or `completed`.
    pub status: String,
    /// Once completed: `success`, `failure`, `skipped`, `cancelled`, ….
    pub conclusion: Option<String>,
    /// When it started.
    pub started_at: Option<u64>,
    /// When it completed.
    pub completed_at: Option<u64>,
}

/// `GET /v1/jobs` query parameters. Jobs come newest first (by ID, which grows with time).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobQuery {
    /// Only this `owner/name`.
    pub repository: Option<String>,
    /// Only jobs in this state.
    pub state: Option<JobState>,
    /// Only jobs with this conclusion, such as `failure`.
    pub conclusion: Option<String>,
    /// Only jobs queued at or after this time.
    pub since: Option<u64>,
    /// Only jobs queued before this time.
    pub until: Option<u64>,
    /// Only jobs with a smaller ID: the `next` of the previous page.
    pub before: Option<u64>,
    /// At most this many jobs; default 50, at most 500.
    pub limit: Option<u32>,
}

/// `GET /v1/jobs`: one page of jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobPage {
    /// Newest first.
    pub jobs: Vec<JobSummary>,
    /// Pass as `before` for the next page; `None` on the last page.
    pub next: Option<u64>,
}

/// `GET /v1/jobs/{id}/logs` query parameters. The response is `text/plain`, one log line per line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogQuery {
    /// Start at this line, 0-based.
    pub from: Option<u64>,
    /// Keep the response open and send lines as they are stored, until the job finishes.
    #[serde(default)]
    pub follow: bool,
}

/// One sample of a container's use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// When it was taken.
    pub at: u64,
    /// CPU use since the previous sample, in millicores.
    pub cpu: u64,
    /// Memory use without page cache.
    pub memory: u64,
    /// Bytes received since the container started.
    pub network_received: u64,
    /// Bytes sent since the container started.
    pub network_sent: u64,
}

/// `GET /v1/jobs/{id}/samples`: what the job's container used over time, from its start to its removal. Samples
/// older than `history.samples` are deleted; the job's metrics stay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleSeries {
    /// The job.
    pub job: u64,
    /// Seconds between samples.
    pub interval: u64,
    /// Oldest first.
    pub samples: Vec<Sample>,
}

/// One sample of the whole slice runners run in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSample {
    /// When it was taken.
    pub at: u64,
    /// CPU use of the slice since the previous sample, in millicores.
    pub cpu: u64,
    /// Memory use of the slice less its inactive page cache, as `docker stats` counts it: a conservative number,
    /// since active page cache still counts, which the high-water mark of measured admission compares to
    /// `MemoryMax`.
    pub memory: u64,
}

/// `GET /v1/host/samples` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSampleQuery {
    /// Only samples taken at or after this time; default the last hour, at most the last 24 hours.
    pub since: Option<u64>,
}

/// `GET /v1/host/samples`: the slice's use over time, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSamples {
    /// Seconds between samples.
    pub interval: u64,
    /// Oldest first.
    pub samples: Vec<HostSample>,
}

/// How `GET /v1/stats` groups jobs in time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Bucket {
    /// By UTC hour.
    Hour,
    /// By UTC day.
    #[default]
    Day,
}

/// `GET /v1/stats` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsQuery {
    /// Only this `owner/name`.
    pub repository: Option<String>,
    /// Only jobs queued at or after this time; default 30 days ago. At most 744 buckets before `until` (or now)
    /// are counted: 31 days of hours, two years of days.
    pub since: Option<u64>,
    /// Only jobs queued before this time.
    pub until: Option<u64>,
    /// The bucket size; default a day.
    #[serde(default)]
    pub bucket: Bucket,
}

/// `GET /v1/stats`: finished jobs per repository and time bucket, for trends. Sums, so clients can average.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// The bucket size.
    pub bucket: Bucket,
    /// By bucket start, then repository.
    pub rows: Vec<StatsRow>,
}

/// The finished jobs of one repository in one bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsRow {
    /// The bucket's start.
    pub start: u64,
    /// `owner/name`.
    pub repository: String,
    /// Jobs completed or lost.
    pub jobs: u64,
    /// Of those, concluded `success`.
    pub succeeded: u64,
    /// Of those, concluded `failure` or `timed_out`, or lost.
    pub failed: u64,
    /// Of those, concluded `cancelled`.
    pub cancelled: u64,
    /// Seconds from queued to started, summed.
    pub queue_seconds: u64,
    /// Seconds from started to finished, summed.
    pub run_seconds: u64,
    /// CPU time, in milliseconds, summed.
    pub cpu_time: u64,
    /// The highest memory peak of a job.
    pub memory_peak: u64,
    /// Bytes received, summed.
    pub network_received: u64,
    /// Bytes from the mirror, summed.
    pub mirror_bytes: u64,
}

/// What a cache volume holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum CacheKind {
    /// A profile's read-only snapshot.
    Snapshot {
        /// The profile.
        profile: String,
    },
    /// A slot's layer changes and Docker Engine data.
    Slot {
        /// The profile.
        profile: String,
        /// 1-based.
        slot: usize,
    },
    /// The pnpm store all slots share.
    Pnpm,
    /// The Maven repository all slots share.
    Maven,
    /// The Docker Hub mirror's storage.
    Mirror,
}

/// One cache volume and its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheVolume {
    /// The Docker volume.
    pub name: String,
    /// What it holds.
    #[serde(flatten)]
    pub kind: CacheKind,
    /// Its size at `measured_at`.
    pub bytes: u64,
    /// The size above which runnerd empties it, or for a snapshot starts it over at its next reseed, if it has one.
    pub limit: Option<u64>,
}

/// `GET /v1/caches`: every cache volume of the configured profiles, as last measured. Docker measures volumes
/// slowly, so runnerd measures them at start and whenever no runner runs after jobs ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caches {
    /// When the sizes were measured; `None` before the first measurement.
    pub measured_at: Option<u64>,
    /// By name.
    pub volumes: Vec<CacheVolume>,
}

/// `GET /v1/caches/history` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHistoryQuery {
    /// Only measurements at or after this time; default 30 days ago.
    pub since: Option<u64>,
}

/// One volume's size at one measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSize {
    /// When it was measured.
    pub at: u64,
    /// The Docker volume.
    pub name: String,
    /// Its size.
    pub bytes: u64,
}

/// `GET /v1/caches/history`: every measurement, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHistory {
    /// Oldest first.
    pub sizes: Vec<CacheSize>,
}
