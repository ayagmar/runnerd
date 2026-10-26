//! Job history as clients read it: one record per GitHub job a runner of this daemon ran, its timeline, steps,
//! metrics and log size; the queries that list them; sample series for graphs; per-repository aggregates; cache
//! sizes. Times are seconds since the Unix epoch, sizes are bytes, CPU is in millicores (1000 = one core busy).

use serde::{Deserialize, Serialize};

mod facts;

pub use facts::{
    JobTitle, Milestone, Outcome, OutcomeKind, Phase, PhaseKind, RunComparison, SampleUsage,
    job_title,
};

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
    /// A runner of another machine took it, or it ended before any runner did, as when a newer push cancelled it.
    /// One no runner took has no `started_at`, except in records of versions that kept the start GitHub gives it.
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

impl Job {
    /// The lines across its memory and CPU charts, as every client labels them: what admission reserved of the
    /// budget, when the record has it, and the cap its container ran with, or for a record without it, `profile`'s
    /// limit as configured now. A reservation of the whole cap is one line.
    pub fn chart_lines(&self, profile: Option<&super::ProfileStatus>) -> ChartLines {
        let limits = self.limits(profile);
        let admission = self.admission.as_ref();
        let name = profile.map_or("profile", |profile| profile.name.as_str());
        ChartLines {
            memory: chart_lines(
                limits.memory,
                admission.map(|admission| admission.reserved),
                |value| crate::config::ByteSize(value).to_string(),
                name,
            ),
            cpu: chart_lines(
                limits.cpu,
                admission
                    .and_then(|admission| admission.cpu)
                    .map(|cpu| cpu.reserved),
                super::cpus,
                name,
            ),
        }
    }

    /// What its container may use of each resource, as every client scales its use against: the cap it ran with,
    /// or, for a record without it, `profile`'s limit as configured now. Each resource falls back on its own, so a
    /// reload that changes the profile never replaces a cap the record has.
    pub fn limits(&self, profile: Option<&super::ProfileStatus>) -> Limits {
        let runner = self.summary.runner.as_ref();
        let limit = |cap: Option<u64>, now: Option<u64>| match (cap, now) {
            (Some(value), _) => Some(Limit {
                value,
                source: LimitSource::Cap,
            }),
            (None, Some(value)) => Some(Limit {
                value,
                source: LimitSource::ProfileNow,
            }),
            (None, None) => None,
        };
        Limits {
            memory: limit(
                runner.and_then(|runner| runner.memory_cap),
                profile.map(|profile| profile.memory),
            ),
            cpu: limit(
                runner.and_then(|runner| runner.cpu_cap),
                profile.map(|profile| u64::from(profile.cpus) * 1000),
            ),
        }
    }
}

/// What a job's container may use, from `Job::limits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// In bytes.
    pub memory: Option<Limit>,
    /// In millicores.
    pub cpu: Option<Limit>,
}

impl Limits {
    /// The limits as one clause, such as `capped at 6G and 4 CPUs`, `capped at 6G; profile now 4 CPUs` or `profile
    /// now 6G and 4 CPUs`; `None` without either.
    pub fn line(&self) -> Option<String> {
        let memory = self.memory.map(|limit| {
            (
                limit.source,
                crate::config::ByteSize(limit.value).to_string(),
            )
        });
        let cpu = self
            .cpu
            .map(|limit| (limit.source, super::cpus(limit.value)));
        let clause = |source: LimitSource, figures: &str| match source {
            LimitSource::Cap => format!("capped at {figures}"),
            LimitSource::ProfileNow => format!("profile now {figures}"),
        };
        match (memory, cpu) {
            (Some((memory_source, memory)), Some((cpu_source, cpu)))
                if memory_source == cpu_source =>
            {
                Some(clause(memory_source, &format!("{memory} and {cpu}")))
            }
            (Some((memory_source, memory)), Some((cpu_source, cpu))) => Some(format!(
                "{}; {}",
                clause(memory_source, &memory),
                clause(cpu_source, &cpu)
            )),
            (Some((source, figure)), None) | (None, Some((source, figure))) => {
                Some(clause(source, &figure))
            }
            (None, None) => None,
        }
    }
}

/// One resource's limit for a job's container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    /// Bytes or millicores.
    pub value: u64,
    /// Where it comes from.
    pub source: LimitSource,
}

/// Where a job's limit comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// The cap its container ran with.
    Cap,
    /// Its profile's limit in the config now, for a record without its container's cap; a reload since may have
    /// changed it.
    ProfileNow,
}

/// One resource's lines: `value` writes a figure, such as `8.5G` or `3.6 CPUs`.
fn chart_lines(
    limit: Option<Limit>,
    reserved: Option<u64>,
    value: fn(u64) -> String,
    profile: &str,
) -> Vec<ChartLine> {
    let line = |figure, kind, label: &str| ChartLine {
        value: figure,
        kind,
        label: format!("{label} {}", value(figure)),
    };
    let mut lines = vec![];
    match limit {
        Some(Limit {
            value: cap,
            source: LimitSource::Cap,
        }) if reserved == Some(cap) => {
            return vec![line(cap, ChartLineKind::CapAndReserved, "cap and reserved")];
        }
        Some(Limit {
            value: cap,
            source: LimitSource::Cap,
        }) => lines.push(line(cap, ChartLineKind::Cap, "cap")),
        Some(Limit {
            value: now,
            source: LimitSource::ProfileNow,
        }) => lines.push(line(
            now,
            ChartLineKind::ProfileNow,
            &format!("{profile} now"),
        )),
        None => {}
    }
    if let Some(reserved) = reserved {
        lines.push(line(reserved, ChartLineKind::Reserved, "reserved"));
    }
    lines
}

/// The lines across a job's memory and CPU charts, from `Job::chart_lines`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartLines {
    /// In bytes.
    pub memory: Vec<ChartLine>,
    /// In millicores.
    pub cpu: Vec<ChartLine>,
}

/// A line across a job's memory or CPU chart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartLine {
    /// Bytes or millicores.
    pub value: u64,
    /// What it stands for.
    pub kind: ChartLineKind,
    /// Its label, such as `cap 8.5G` or `reserved 3.6 CPUs`.
    pub label: String,
}

/// What a line across a job's chart stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartLineKind {
    /// The most its container may use: its cap.
    Cap,
    /// Its profile's limit in the config now, for a record without its container's cap; a reload since may have
    /// changed it.
    ProfileNow,
    /// What admission counted against the budget for its runner.
    Reserved,
    /// A reservation of the whole cap, as admission by caps makes.
    CapAndReserved,
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

impl Timeline {
    /// When the runner that took it began to start for it: its container's start, or the job's queueing when that
    /// container waited for jobs before, as a warm runner does. `None` until a runner took it.
    pub fn runner_from(&self) -> Option<u64> {
        self.container_started_at
            .map(|started| started.max(self.queued_at))
    }
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
    /// What the shared build cache served it and took from it.
    pub build_cache: BuildCacheCounts,
    /// Times the kernel held its container's sockets' receive windows small because the container's memory was
    /// under pressure (`memory.events` `sock_throttled`): reclaim went badly, mostly because the host was short of
    /// memory, and its downloads slowed. `None` when the kernel does not count it, or in records from before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sock_throttled: Option<u64>,
}

/// What the shared build cache served one runner and took from it (ADR 0052), counted by the cache itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildCacheCounts {
    /// Lookups that found an entry.
    pub hits: u64,
    /// Lookups that found none.
    pub misses: u64,
    /// Bytes of the entries it served.
    pub read_bytes: u64,
    /// Bytes of the entries it stored.
    pub written_bytes: u64,
    /// Entries it was sent and did not store: the job may not write (a pull request, another branch), or the
    /// cache was full.
    pub refused: u64,
    /// Whether the job could write: a push, scheduled or manually started run of a branch that may write.
    pub writer: bool,
}

impl BuildCacheCounts {
    /// Hits and misses.
    pub fn lookups(&self) -> u64 {
        self.hits + self.misses
    }

    /// Hits in percent of lookups, `None` without any.
    pub fn hit_percent(&self) -> Option<u64> {
        let lookups = self.lookups();
        (lookups > 0).then(|| self.hits * 100 / lookups)
    }

    /// Adds `other`'s counts; `writer` stays.
    pub fn add(&mut self, other: &BuildCacheCounts) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.read_bytes += other.read_bytes;
        self.written_bytes += other.written_bytes;
        self.refused += other.refused;
    }
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
    /// Only jobs of the workflow with this name, such as `CI`.
    pub workflow: Option<String>,
    /// Only jobs with this name in their workflow, such as `build`.
    pub name: Option<String>,
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

/// The host's memory at one moment, beside the slice's: what admission fits new runners in besides `MemoryMax`
/// (ADR 0055). Read every few seconds; not kept in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMemory {
    /// When it was read.
    pub at: u64,
    /// `MemTotal` in `/proc/meminfo`, in bytes.
    pub total: u64,
    /// `MemAvailable`: what the host could give without swapping, page cache it can drop included, in bytes.
    pub available: u64,
    /// The slice's memory without page cache and reclaimable slab: what the runners hold that `MemAvailable` leaves
    /// out.
    pub slice: u64,
    /// `/proc/pressure/memory`'s `some` line: the share of time at least one task waited for memory. `None` on a
    /// kernel without pressure stall information.
    pub some: Option<Pressure>,
    /// Its `full` line: the share of time every task that could run waited for memory.
    pub full: Option<Pressure>,
    /// The programs outside the slice that hold the most memory, largest first: at most a few, none under a floor.
    /// Read less often than the rest. Empty when none is that large.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holders: Vec<Holder>,
}

impl HostMemory {
    /// What the host holds outside the slice's runners, in bytes: its other programs, tmpfs and the kernel, without
    /// page cache it can drop.
    pub fn outside(&self) -> u64 {
        self.total
            .saturating_sub(self.available)
            .saturating_sub(self.slice)
    }

    /// What holds the host's memory outside the slice, as the clients say it: `programs outside CI hold 30G` and,
    /// with `holders`, `, most java 9.3G and qemu-system-x86 5.8G`.
    pub fn outside_line(&self) -> String {
        let size = |bytes: u64| crate::config::ByteSize(bytes).to_string();
        let line = format!("programs outside CI hold {}", size(self.outside()));
        let named: Vec<String> = self
            .holders
            .iter()
            .map(|holder| format!("{} {}", holder.name, size(holder.memory)))
            .collect();
        match named.as_slice() {
            [] => line,
            [one] => format!("{line}, most {one}"),
            [rest @ .., last] => format!("{line}, most {} and {last}", rest.join(", ")),
        }
    }
}

/// A program outside the slice and the memory its processes hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// Its processes' name, as `/proc/<pid>/comm` says it: never a command line, argument or path.
    pub name: String,
    /// What its processes hold that the host can't drop as page cache (`RssAnon` and `RssShmem`), in bytes.
    pub memory: u64,
}

/// One line of a pressure stall file, in hundredths of a percent: 250 is 2.5% of the time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pressure {
    /// Over the last 10 seconds.
    pub avg10: u32,
    /// Over the last 60 seconds.
    pub avg60: u32,
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

/// `GET /v1/activity` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityQuery {
    /// The start of the window; default 24 hours ago. It ends now.
    pub since: Option<u64>,
}

/// `GET /v1/activity`: every job runnerd's runners started or finished in a window, by when that happened rather
/// than when the job was queued, so a job that waited or ran long counts in the window it started or ended in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// The start of the window.
    pub since: u64,
    /// When it was read: the end of the window.
    pub until: u64,
    /// Each job that started in the window, oldest start first.
    pub started: Vec<Started>,
    /// The jobs that finished in the window: completed, or lost with their runner.
    pub finished: u64,
    /// Of those, the ones that failed, timed out or were lost, as `GET /v1/stats` counts failures.
    pub failed: u64,
    /// Of the failed, the ones their runner failed rather than their own steps: lost with their runner, or failed
    /// after the kernel killed a process of theirs for memory at their cap.
    pub runner_failed: u64,
}

impl Activity {
    /// The 95th percentile of the waits of the jobs that started, by nearest rank as sizing's are; `None` when none
    /// started.
    pub fn wait_p95(&self) -> Option<u64> {
        let mut waits: Vec<u64> = self.started.iter().map(|start| start.waited).collect();
        waits.sort_unstable();
        let rank = (waits.len() * 95).div_ceil(100);
        waits.get(rank.checked_sub(1)?).copied()
    }

    /// The longest wait of the jobs that started in each of the 24 hours before `until`, oldest hour first; 0 for
    /// an hour without a start. Counted back from `until`, so the hours are the day the other figures count.
    pub fn wait_hours(&self) -> [u64; 24] {
        const HOUR: u64 = 3600;
        let since = self.until.saturating_sub(24 * HOUR);
        let mut hours = [0u64; 24];
        for start in self
            .started
            .iter()
            .filter(|start| (since..=self.until).contains(&start.at))
        {
            // A start exactly 24 hours ago is in the oldest hour.
            let back =
                usize::try_from((self.until - start.at) / HOUR).map_or(23, |back| back.min(23));
            hours[23 - back] = hours[23 - back].max(start.waited);
        }
        hours
    }
}

/// A job that started: when, and how long it had waited since it was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Started {
    /// When it started.
    pub at: u64,
    /// Seconds it waited.
    pub waited: u64,
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
    /// The build cache every runner shares: a directory of the daemon's, not a Docker volume.
    Build,
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
    /// The shared build cache now, when it is on.
    pub build: Option<BuildCacheStats>,
}

/// The shared build cache (ADR 0052): its size now, and what it served the jobs of the last 7 days.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildCacheStats {
    /// The entries' bytes.
    pub bytes: u64,
    /// How many entries it holds.
    pub entries: u64,
    /// `[caches.build] max_size`.
    pub limit: u64,
    /// The runners of the last 7 days, summed.
    pub week: BuildCacheCounts,
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
