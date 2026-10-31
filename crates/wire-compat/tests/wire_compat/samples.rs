//! One sample of every JSON document the daemon sends, a client sends or the history stores, built from this
//! version's types. `wire_compat` checks that the previous release reads them, and writes them out as that
//! release's fixtures when a release is cut.
//!
//! The samples must reach every field and every variant, or a break there goes unseen:
//!
//! - Struct literals name every field, never `..Default::default()`, so a new field does not compile until it has
//!   a value here. A full sample sets every `Option` and fills every `Vec`; an `_empty` sample leaves out what
//!   serializing may skip.
//! - Every enum gets one item per variant, and a `match` that fails to compile when a variant is added.

use runnerd_core::api::{
    Activity, ActivityQuery, Admission, AdmissionPolicy, Advice, AutoNote, AutoPlan, Basis, Bucket,
    Budget, BuildCacheCounts, BuildCacheStats, Busy, CacheHistory, CacheHistoryQuery, CacheKind,
    CacheSize, CacheVolume, Caches, Check, CheckStatus, ConfigCheck, ConfigFile, CpuAdmission,
    CpuBudget, Dashboard, Delivery, Digest, Doctor, ErrorResponse, Event, EventEnvelope,
    EventQuery, HoldCause, Holder, HostBudget, HostMemory, HostSample, HostSampleQuery,
    HostSamples, Insight, InsightKind, Insights, InsightsQuery, Job, JobMetrics, JobPage, JobPeak,
    JobQuery, JobRunner, JobSizing, JobState, JobSummary, LogQuery, LogSummary, LongWait,
    MemoryBound, MemoryHold, Notification, NotifyEvent, NotifyTest, NotifyTestQuery, Pairing,
    PairingQuery, Pairings, PauseQuery, Paused, Peaks, Period, PoolStatus, Pressure, PricedRunner,
    Priority, ProfileSizing, ProfileStatus, QueuedJob, Record, RecordLimits, RecordNote,
    RecordRemoval, Records, RemoveRecordsQuery, RemovedRecords, ReplaceQuery, RepositoryStatus,
    RepositoryWaits, Resize, RevertCause, RunLink, RunnerFailure, RunnerStatus, RunningJob, Sample,
    SampleSeries, SavedWindow, Savings, SeedMemory, Severity, ShellMemory, ShellQuery,
    ShutdownAnswer, ShutdownRequest, Sizing, SizingChange, SizingHold, SizingQuery, SizingRefused,
    SizingRevert, Sku, SlotStatus, Spread, Started, Starts, Stats, StatsQuery, StatsRow, Status,
    Step, Stopping, Timeline, Usd, WaitSpread, Waiting, WeekWaits,
};

pub(crate) fn status() -> Status {
    Status {
        version: "0.0.0".to_owned(),
        shutting_down: true,
        stopping: Some(Stopping::LeavesJobs),
        budget: budget(),
        profiles: vec![profile_status()],
        repositories: vec![repository_status()],
        queued: vec![queued_job()],
    }
}

/// A status with nothing that serializing skips: a host reading that names no programs and has no ceiling, no CPU
/// budget, a queued job without a pool or a reason.
pub(crate) fn status_empty() -> Status {
    let mut status = status();
    if let Some(host) = &mut status.budget.host {
        host.memory.holders = Vec::new();
        host.memory.ceiling = None;
    }
    status.budget.cpu = None;
    status.budget.slice_cpus = None;
    status.queued[0].profile = None;
    status.queued[0].waiting = None;
    status.stopping = None;
    status
}

fn budget() -> Budget {
    Budget {
        slice: "runnerd.slice".to_owned(),
        memory_max: Some(32 << 30),
        memory_in_use: 9 << 30,
        admission: AdmissionPolicy::Measured,
        held: false,
        mirror_memory: 1 << 30,
        seed_memory: vec![SeedMemory {
            profile: "halyard".to_owned(),
            memory: 2 << 30,
        }],
        shell_memory: vec![ShellMemory {
            job: 41,
            memory: 1 << 30,
        }],
        host: Some(HostBudget {
            memory: host_memory(),
            limit: 40 << 30,
            spare: 12 << 30,
            stalled: false,
        }),
        cpu: Some(CpuBudget {
            cpus: 16_000,
            budget: 24_000,
            in_use: 8_000,
            held: false,
        }),
        slice_cpus: Some(16_000),
    }
}

fn host_memory() -> HostMemory {
    HostMemory {
        at: 1_761_400_000,
        total: 64 << 30,
        available: 31 << 30,
        slice: 9 << 30,
        some: Some(Pressure {
            avg10: 250,
            avg60: 120,
        }),
        full: Some(Pressure {
            avg10: 150,
            avg60: 60,
        }),
        holders: vec![Holder {
            name: "java".to_owned(),
            memory: 9 << 30,
        }],
        ceiling: Some(96 << 30),
    }
}

fn profile_status() -> ProfileStatus {
    ProfileStatus {
        name: "halyard".to_owned(),
        image: "runnerd-runner:base".to_owned(),
        cpus: 4,
        memory: 8 << 30,
        reseeding: false,
        slots: vec![slot_status()],
    }
}

fn slot_status() -> SlotStatus {
    SlotStatus {
        slot: 0,
        runner: Some(RunnerStatus {
            repository: "tidewater/halyard".to_owned(),
            name: "runnerd-halyard-0".to_owned(),
            runner_id: 7,
            container: "runnerd-halyard-0".to_owned(),
            busy: true,
            started_at: 1_761_400_000,
            reserved: 6 << 30,
            warm: true,
        }),
    }
}

fn repository_status() -> RepositoryStatus {
    RepositoryStatus {
        name: "tidewater/halyard".to_owned(),
        pools: vec![PoolStatus {
            profile: "halyard".to_owned(),
            labels: vec!["self-hosted".to_owned()],
            queued_jobs: 1,
        }],
        paused: false,
        queued_jobs: 1,
    }
}

fn queued_job() -> QueuedJob {
    QueuedJob {
        id: 42,
        repository: "tidewater/halyard".to_owned(),
        profile: Some("halyard".to_owned()),
        run_id: 900,
        run_attempt: 1,
        workflow: Some("ci".to_owned()),
        name: "build".to_owned(),
        branch: Some("main".to_owned()),
        url: Some("https://github.com/tidewater/halyard/actions/runs/900".to_owned()),
        queued_at: 1_761_400_000,
        waiting: Some(Waiting::Memory {
            needs: 6 << 30,
            free: 2 << 30,
        }),
    }
}

pub(crate) fn waiting() -> Vec<Waiting> {
    let every = vec![
        Waiting::Paused,
        Waiting::Backoff {
            until: 1_761_400_300,
            cause: HoldCause::Start,
        },
        Waiting::RunnerStarting,
        Waiting::Reseeding {
            profile: "halyard".to_owned(),
        },
        Waiting::PullingImage {
            profile: "halyard".to_owned(),
        },
        Waiting::NoSlot {
            profile: "halyard".to_owned(),
        },
        Waiting::Memory { needs: 6, free: 2 },
        Waiting::HostMemory { needs: 6, free: 2 },
        Waiting::Cpu { needs: 4, free: 1 },
        Waiting::BehindCpu { job: 41 },
        Waiting::BehindStart { job: 41 },
        Waiting::NoPool {
            labels: vec!["halyard-lite".to_owned()],
        },
    ];
    for waiting in &every {
        match waiting {
            Waiting::Paused
            | Waiting::Backoff { .. }
            | Waiting::RunnerStarting
            | Waiting::Reseeding { .. }
            | Waiting::PullingImage { .. }
            | Waiting::NoSlot { .. }
            | Waiting::Memory { .. }
            | Waiting::HostMemory { .. }
            | Waiting::Cpu { .. }
            | Waiting::BehindCpu { .. }
            | Waiting::BehindStart { .. }
            | Waiting::NoPool { .. }
            // What a client reads a later daemon's reason as; no daemon sends it, so it has no sample.
            | Waiting::Unknown => {}
        }
    }
    every
}

pub(crate) fn hold_cause() -> Vec<HoldCause> {
    let every = vec![
        HoldCause::NoJob,
        HoldCause::IdleExits,
        HoldCause::Register,
        HoldCause::Start,
    ];
    for cause in &every {
        match cause {
            HoldCause::NoJob | HoldCause::IdleExits | HoldCause::Register | HoldCause::Start => {}
        }
    }
    every
}

pub(crate) fn admission_policy() -> Vec<AdmissionPolicy> {
    let every = vec![AdmissionPolicy::Caps, AdmissionPolicy::Measured];
    for policy in &every {
        match policy {
            AdmissionPolicy::Caps | AdmissionPolicy::Measured => {}
        }
    }
    every
}

pub(crate) fn basis() -> Vec<Basis> {
    let every = vec![
        Basis::Job {
            runs: 12,
            peak: JobPeak::Largest,
        },
        Basis::Profile { runs: 30 },
        Basis::ProfileMax { runs: 30 },
        Basis::Overran {
            until: 1_761_400_300,
        },
        Basis::Cap,
        Basis::Unpredictable {
            until: 1_761_400_300,
        },
        Basis::Unmeasured,
    ];
    for basis in &every {
        match basis {
            Basis::Job { .. }
            | Basis::Profile { .. }
            | Basis::ProfileMax { .. }
            | Basis::Overran { .. }
            | Basis::Cap
            | Basis::Unpredictable { .. }
            | Basis::Unmeasured => {}
        }
    }
    every
}

pub(crate) fn job_peak() -> Vec<JobPeak> {
    let every = vec![JobPeak::P95, JobPeak::Largest];
    for peak in &every {
        match peak {
            JobPeak::P95 | JobPeak::Largest => {}
        }
    }
    every
}

pub(crate) fn admission() -> Admission {
    Admission {
        policy: AdmissionPolicy::Measured,
        reserved: 6 << 30,
        basis: Basis::Job {
            runs: 12,
            peak: JobPeak::P95,
        },
        free: Some(12 << 30),
        slice: Some(32 << 30),
        cpu: Some(CpuAdmission {
            reserved: 2_000,
            basis: Basis::Profile { runs: 30 },
            free: 8_000,
            waited: Some(30),
        }),
    }
}

/// An admission with nothing that serializing skips: no CPU admission, and a job basis on its p95.
pub(crate) fn admission_empty() -> Admission {
    Admission {
        cpu: None,
        ..admission()
    }
}

pub(crate) fn running_job() -> RunningJob {
    RunningJob {
        repository: "tidewater/halyard".to_owned(),
        runner: "runnerd-halyard-0".to_owned(),
        job: Some(42),
    }
}

pub(crate) fn busy() -> Busy {
    Busy {
        message: "tidewater/halyard runs a job".to_owned(),
        jobs: vec![running_job()],
    }
}

pub(crate) fn paused() -> Paused {
    Paused {
        stopped: vec![running_job()],
    }
}

pub(crate) fn config_file() -> ConfigFile {
    ConfigFile {
        path: "/home/runner/.config/runnerd/runnerd.toml".to_owned(),
        text: "[host]\n".to_owned(),
        revision: "3f2a".to_owned(),
    }
}

pub(crate) fn config_check() -> ConfigCheck {
    ConfigCheck {
        valid: true,
        message: "the config is valid".to_owned(),
        warnings: vec!["a warning".to_owned()],
        admission: AdmissionPolicy::Measured,
        memory_caps: Some(16 << 30),
        memory_needed: Some(12 << 30),
        memory_max: Some(32 << 30),
        revision: Some("3f2a".to_owned()),
    }
}

pub(crate) fn doctor() -> Doctor {
    let checks = check_status()
        .into_iter()
        .map(|status| Check {
            name: "docker".to_owned(),
            status,
            detail: "Docker 29.1".to_owned(),
            fix: Some("start Docker".to_owned()),
        })
        .collect();
    Doctor { checks }
}

fn check_status() -> Vec<CheckStatus> {
    let every = vec![
        CheckStatus::Pass,
        CheckStatus::Fail,
        CheckStatus::Warn,
        CheckStatus::Skip,
        CheckStatus::Info,
    ];
    for status in &every {
        match status {
            CheckStatus::Pass
            | CheckStatus::Fail
            | CheckStatus::Warn
            | CheckStatus::Skip
            | CheckStatus::Info => {}
        }
    }
    every
}

pub(crate) fn dashboard() -> Dashboard {
    Dashboard {
        open: Some("http://127.0.0.1:7341/".to_owned()),
    }
}

pub(crate) fn pairings() -> Pairings {
    Pairings {
        links: vec![Pairing {
            origin: "https://ci.tidewater.example".to_owned(),
            url: "https://ci.tidewater.example/pair#code".to_owned(),
            lifetime: 300,
        }],
    }
}

pub(crate) fn stopping() -> Vec<Stopping> {
    let every = vec![
        Stopping::WaitsForJobs,
        Stopping::LeavesJobs,
        Stopping::KillsJobs,
    ];
    for stopping in &every {
        match stopping {
            Stopping::WaitsForJobs
            | Stopping::LeavesJobs
            | Stopping::KillsJobs
            // What a client reads a later daemon's kind of stop as; no daemon sends it, so it has no sample.
            | Stopping::Unknown => {}
        }
    }
    every
}

pub(crate) fn shutdown_answer() -> ShutdownAnswer {
    ShutdownAnswer {
        stopping: Stopping::KillsJobs,
    }
}

pub(crate) fn shutdown_request() -> ShutdownRequest {
    ShutdownRequest { force: true }
}

pub(crate) fn error_response() -> ErrorResponse {
    ErrorResponse {
        message: "no such job".to_owned(),
    }
}

pub(crate) fn job_summary() -> JobSummary {
    JobSummary {
        id: 42,
        repository: "tidewater/halyard".to_owned(),
        run_id: 900,
        run_attempt: 1,
        workflow: Some("ci".to_owned()),
        name: "build".to_owned(),
        branch: Some("main".to_owned()),
        url: Some("https://github.com/tidewater/halyard/actions/runs/900".to_owned()),
        labels: vec!["self-hosted".to_owned()],
        state: JobState::Completed,
        conclusion: Some("success".to_owned()),
        runner: Some(JobRunner {
            name: "runnerd-halyard-0".to_owned(),
            profile: "halyard".to_owned(),
            slot: 0,
            memory_cap: Some(8 << 30),
            cpu_cap: Some(4_000),
        }),
        timeline: Timeline {
            queued_at: 1_761_400_000,
            container_started_at: Some(1_761_400_010),
            started_at: Some(1_761_400_020),
            finished_at: Some(1_761_400_600),
            cleaned_at: Some(1_761_400_610),
            unseen: 15,
        },
        metrics: JobMetrics {
            cpu_time: Some(1_200_000),
            cpu_peak: Some(3_900),
            cpu_average: Some(2_000),
            memory_peak: Some(5 << 30),
            memory_average: Some(3 << 30),
            network_received: Some(200 << 20),
            network_sent: Some(10 << 20),
            mirror_bytes: 150 << 20,
            build_cache: build_cache_counts(),
            sock_throttled: Some(2),
            cpu_periods: Some(5_800),
            cpu_throttled: Some(900),
            cpu_stalled: Some(140_000),
        },
        log: LogSummary {
            lines: 1_200,
            bytes: 90_000,
            truncated: false,
        },
    }
}

pub(crate) fn job_state() -> Vec<JobState> {
    let every = vec![
        JobState::Queued,
        JobState::Running,
        JobState::Completed,
        JobState::Lost,
        JobState::Elsewhere,
    ];
    for state in &every {
        match state {
            JobState::Queued
            | JobState::Running
            | JobState::Completed
            | JobState::Lost
            | JobState::Elsewhere => {}
        }
    }
    every
}

fn build_cache_counts() -> BuildCacheCounts {
    BuildCacheCounts {
        hits: 80,
        misses: 20,
        read_bytes: 300 << 20,
        written_bytes: 40 << 20,
        refused: 1,
        writer: true,
    }
}

fn step() -> Step {
    Step {
        number: 1,
        name: "Set up job".to_owned(),
        status: "completed".to_owned(),
        conclusion: Some("success".to_owned()),
        started_at: Some(1_761_400_020),
        completed_at: Some(1_761_400_025),
    }
}

pub(crate) fn job() -> Job {
    Job {
        summary: job_summary(),
        steps: vec![step()],
        record: Some(RecordNote {
            created_at: Some(1_761_400_610),
            bytes: Some(2 << 30),
            skipped: Some("the job succeeded".to_owned()),
            removed_at: Some(1_761_500_000),
            removed_by: Some(RecordRemoval::User),
        }),
        admission: Some(admission()),
    }
}

/// A job with nothing that serializing skips: no record or admission, metrics without throttling or CPU holds.
pub(crate) fn job_empty() -> Job {
    let mut job = job();
    job.record = None;
    job.admission = None;
    job.summary.metrics.sock_throttled = None;
    job.summary.metrics.cpu_periods = None;
    job.summary.metrics.cpu_throttled = None;
    job.summary.metrics.cpu_stalled = None;
    job
}

pub(crate) fn job_page() -> JobPage {
    JobPage {
        jobs: vec![job_summary()],
        next: Some(41),
    }
}

fn sample() -> Sample {
    Sample {
        at: 1_761_400_100,
        cpu: 3_000,
        memory: 4 << 30,
        network_received: 1 << 20,
        network_sent: 1 << 10,
    }
}

pub(crate) fn sample_series() -> SampleSeries {
    SampleSeries {
        job: 42,
        interval: 5,
        samples: vec![sample()],
    }
}

fn host_sample() -> HostSample {
    HostSample {
        at: 1_761_400_100,
        cpu: 9_000,
        memory: 20 << 30,
    }
}

pub(crate) fn host_samples() -> HostSamples {
    HostSamples {
        interval: 10,
        samples: vec![host_sample()],
    }
}

pub(crate) fn bucket() -> Vec<Bucket> {
    let every = vec![Bucket::Hour, Bucket::Day];
    for bucket in &every {
        match bucket {
            Bucket::Hour | Bucket::Day => {}
        }
    }
    every
}

pub(crate) fn activity() -> Activity {
    Activity {
        since: 1_761_300_000,
        until: 1_761_400_000,
        started: vec![Started {
            at: 1_761_350_000,
            waited: 30,
        }],
        finished: 12,
        failed: 1,
        runner_failed: 1,
    }
}

pub(crate) fn stats() -> Stats {
    Stats {
        bucket: Bucket::Day,
        rows: vec![StatsRow {
            start: 1_761_350_400,
            repository: "tidewater/halyard".to_owned(),
            jobs: 12,
            succeeded: 10,
            failed: 1,
            cancelled: 1,
            queue_seconds: 300,
            run_seconds: 7_200,
            cpu_time: 14_400_000,
            memory_peak: 5 << 30,
            network_received: 2 << 30,
            mirror_bytes: 1 << 30,
        }],
    }
}

pub(crate) fn caches() -> Caches {
    let volumes = cache_kind()
        .into_iter()
        .map(|kind| CacheVolume {
            name: "runnerd-cache".to_owned(),
            kind,
            bytes: 3 << 30,
            limit: Some(10 << 30),
        })
        .collect();
    Caches {
        measured_at: Some(1_761_400_000),
        volumes,
        build: Some(BuildCacheStats {
            bytes: 4 << 30,
            entries: 9_000,
            limit: 20 << 30,
            week: build_cache_counts(),
            actions_cache: true,
        }),
    }
}

fn cache_kind() -> Vec<CacheKind> {
    let every = vec![
        CacheKind::Snapshot {
            profile: "halyard".to_owned(),
        },
        CacheKind::Slot {
            profile: "halyard".to_owned(),
            slot: 0,
        },
        CacheKind::Pnpm,
        CacheKind::Maven,
        CacheKind::Mirror,
        CacheKind::Build,
    ];
    for kind in &every {
        match kind {
            CacheKind::Snapshot { .. }
            | CacheKind::Slot { .. }
            | CacheKind::Pnpm
            | CacheKind::Maven
            | CacheKind::Mirror
            | CacheKind::Build => {}
        }
    }
    every
}

pub(crate) fn cache_history() -> CacheHistory {
    CacheHistory {
        sizes: vec![CacheSize {
            at: 1_761_400_000,
            name: "runnerd-cache".to_owned(),
            bytes: 3 << 30,
        }],
    }
}

fn record() -> Record {
    Record {
        job: 42,
        repository: "tidewater/halyard".to_owned(),
        profile: "halyard".to_owned(),
        workflow: Some("ci".to_owned()),
        name: Some("build".to_owned()),
        created_at: 1_761_400_610,
        bytes: 2 << 30,
        image: "runnerd-record-42".to_owned(),
    }
}

pub(crate) fn records() -> Records {
    Records {
        records: vec![record()],
        limits: RecordLimits {
            max_total: 20 << 30,
            max_count: 10,
            max_age: 7 * 86_400,
            min_free: 50 << 30,
        },
        shells: vec![42],
    }
}

pub(crate) fn record_removal() -> Vec<RecordRemoval> {
    let every = vec![
        RecordRemoval::Count,
        RecordRemoval::Size,
        RecordRemoval::Age,
        RecordRemoval::User,
    ];
    for removal in &every {
        match removal {
            RecordRemoval::Count
            | RecordRemoval::Size
            | RecordRemoval::Age
            | RecordRemoval::User => {}
        }
    }
    every
}

pub(crate) fn removed_records() -> RemovedRecords {
    RemovedRecords {
        jobs: vec![42],
        bytes: 2 << 30,
    }
}

fn spread() -> Spread {
    Spread {
        p50: 3 << 30,
        p95: 5 << 30,
        p99: 6 << 30,
        max: 7 << 30,
    }
}

fn peaks() -> Peaks {
    Peaks {
        jobs: 30,
        at_cap: 1,
        on_cap: 1,
        memory: spread(),
        cpu: spread(),
        cpu_runs: 20,
    }
}

fn resize() -> Resize {
    Resize {
        memory: 8 << 30,
        slots: 2,
        cpus: Some(4),
    }
}

fn sizing_change() -> SizingChange {
    SizingChange {
        at: 1_761_400_000,
        profile: "halyard".to_owned(),
        from: resize(),
        to: Resize {
            memory: 10 << 30,
            ..resize()
        },
        reason: "its p95 rose".to_owned(),
        uncertain: true,
    }
}

pub(crate) fn sizing() -> Sizing {
    let jobs = advice()
        .into_iter()
        .map(|advice| JobSizing {
            repository: "tidewater/halyard".to_owned(),
            workflow: Some("ci".to_owned()),
            name: "build".to_owned(),
            peaks: peaks(),
            wait: spread(),
            advice,
        })
        .collect();
    Sizing {
        since: 1_760_800_000,
        until: 1_761_400_000,
        margin: 20,
        sample_interval: 5,
        min_jobs: 10,
        memory_max: Some(32 << 30),
        admission: AdmissionPolicy::Measured,
        profiles: vec![ProfileSizing {
            name: "halyard".to_owned(),
            memory: 8 << 30,
            cpus: 4,
            slots: 2,
            peaks: peaks(),
            wait: spread(),
            slot_wait: spread(),
            starts: Starts {
                warm: 10,
                warm_p50: 5,
                cold: 3,
                cold_p50: 40,
            },
            busy: 1,
            wants_slots: true,
            advice: Advice::Keep { memory: 8 << 30 },
            jobs,
            revert: Some(SizingRevert {
                from: 6 << 30,
                to: 8 << 30,
                job: "tidewater/halyard ci / build".to_owned(),
                cause: RevertCause::Failed { runs: 3 },
            }),
            hold: Some(MemoryHold {
                need: 10 << 30,
                bound: 8 << 30,
                by: MemoryBound::Ceiling {
                    ceiling: 9 << 30,
                    mirror: 512 << 20,
                },
            }),
        }],
        auto: true,
        changes: vec![sizing_change()],
        auto_plan: Some(AutoPlan {
            every: 86_400,
            interval: 3_600,
            raise_interval: 600,
            max_step: 25,
            next: vec![sizing_change()],
            held: vec![SizingHold {
                profile: "halyard".to_owned(),
                changed: 1_761_300_000,
                until: 1_761_500_000,
                lower_until: 1_761_900_000,
            }],
            blocked: Some("the slice is full".to_owned()),
            refused: vec![SizingRefused {
                profile: "halyard".to_owned(),
                reason: "the CPU budget is full".to_owned(),
            }],
            refused_slots: vec![SizingRefused {
                profile: "halyard".to_owned(),
                reason: "it is at its max_slots, 4".to_owned(),
            }],
            waited_before_change: vec!["halyard".to_owned()],
        }),
    }
}

/// A sizing with nothing that serializing skips: no auto plan, resizes without CPUs.
pub(crate) fn sizing_empty() -> Sizing {
    let mut sizing = sizing();
    sizing.auto_plan = None;
    for change in &mut sizing.changes {
        change.from.cpus = None;
        change.to.cpus = None;
    }
    sizing
}

fn advice() -> Vec<Advice> {
    let every = vec![
        Advice::TooFew { jobs: 3 },
        Advice::Keep { memory: 8 << 30 },
        Advice::Lower {
            memory: 6 << 30,
            slots: 3,
        },
        Advice::Raise {
            memory: 10 << 30,
            slots: 2,
        },
        Advice::SliceTooSmall { memory: 40 << 30 },
    ];
    for advice in &every {
        match advice {
            Advice::TooFew { .. }
            | Advice::Keep { .. }
            | Advice::Lower { .. }
            | Advice::Raise { .. }
            | Advice::SliceTooSmall { .. } => {}
        }
    }
    every
}

pub(crate) fn memory_bound() -> Vec<MemoryBound> {
    let every = vec![
        MemoryBound::Ceiling {
            ceiling: 9 << 30,
            mirror: 512 << 20,
        },
        MemoryBound::MaxMemory,
        MemoryBound::MinMemory,
    ];
    for bound in &every {
        match bound {
            MemoryBound::Ceiling { .. } | MemoryBound::MaxMemory | MemoryBound::MinMemory => {}
        }
    }
    every
}

fn run_link() -> RunLink {
    RunLink {
        job: 42,
        run_id: 900,
        run_attempt: 1,
        branch: Some("main".to_owned()),
        url: Some("https://github.com/tidewater/halyard/actions/runs/900".to_owned()),
        at: 1_761_400_000,
    }
}

/// One insight of each kind.
fn insights_of_every_kind() -> Vec<Insight> {
    insight_kind()
        .into_iter()
        .map(|kind| Insight {
            key: "tidewater/halyard:ci:build".to_owned(),
            severity: Severity::Warning,
            repository: "tidewater/halyard".to_owned(),
            workflow: Some("ci".to_owned()),
            job: Some("build".to_owned()),
            sentence: "build got slower".to_owned(),
            kind,
            runs: vec![run_link()],
        })
        .collect()
}

fn insight_kind() -> Vec<InsightKind> {
    let every = vec![
        InsightKind::Slower {
            branch: Some("main".to_owned()),
            before: 300,
            now: 600,
            since: Some(run_link()),
        },
        InsightKind::Flaky {
            flaky: 2,
            total: 20,
        },
        InsightKind::NearCap {
            profile: "halyard".to_owned(),
            peak: 7 << 30,
            cap: 8 << 30,
            auto: AutoNote::Raises,
        },
        InsightKind::ColdCache {
            branch: Some("main".to_owned()),
            hits: 2,
            lookups: 100,
        },
        InsightKind::Throttled {
            throttled: 3,
            total: 20,
        },
        InsightKind::Waits {
            before: 30,
            now: 300,
        },
        InsightKind::CpuHeld {
            profile: "halyard".to_owned(),
            cpus: 9,
            held: 3,
            total: 10,
            raised_from: Some(6),
            set: Some(14),
        },
    ];
    for kind in &every {
        match kind {
            InsightKind::Slower { .. }
            | InsightKind::Flaky { .. }
            | InsightKind::NearCap { .. }
            | InsightKind::ColdCache { .. }
            | InsightKind::Throttled { .. }
            | InsightKind::Waits { .. }
            | InsightKind::CpuHeld { .. }
            // A reader's fallback for a kind from a later release: the daemon never writes it, so no sample.
            | InsightKind::Unknown => {}
        }
    }
    every
}

pub(crate) fn severity() -> Vec<Severity> {
    let every = vec![Severity::Warning, Severity::Notice];
    for severity in &every {
        match severity {
            Severity::Warning | Severity::Notice => {}
        }
    }
    every
}

pub(crate) fn auto_note() -> Vec<AutoNote> {
    let every = vec![AutoNote::Raises, AutoNote::AtMax, AutoNote::Off];
    for note in &every {
        match note {
            AutoNote::Raises | AutoNote::AtMax | AutoNote::Off => {}
        }
    }
    every
}

fn week_waits() -> WeekWaits {
    let spread = WaitSpread {
        jobs: 20,
        p50: Some(10),
        p95: Some(120),
    };
    WeekWaits {
        all: spread.clone(),
        warm: spread.clone(),
        cold: spread,
    }
}

pub(crate) fn insights() -> Insights {
    Insights {
        since: 1_760_800_000,
        until: 1_761_400_000,
        insights: insights_of_every_kind(),
        waits: vec![RepositoryWaits {
            repository: "tidewater/halyard".to_owned(),
            now: week_waits(),
            before: week_waits(),
        }],
    }
}

pub(crate) fn digest() -> Digest {
    Digest {
        since: 1_760_800_000,
        until: 1_761_400_000,
        finished: 120,
        failed: 4,
        new: insights_of_every_kind(),
        waits: vec![LongWait {
            job: 42,
            repository: "tidewater/halyard".to_owned(),
            workflow: Some("ci".to_owned()),
            name: "build".to_owned(),
            waited: 900,
            url: Some("https://github.com/tidewater/halyard/actions/runs/900".to_owned()),
        }],
        savings: Some(saved_window(Period::Month)),
    }
}

fn saved_window(period: Period) -> SavedWindow {
    SavedWindow {
        period,
        since: 1_759_276_800,
        cut: true,
        jobs: 12,
        minutes: 340,
        cost: Usd(4_020_000),
        runners: vec![
            PricedRunner {
                sku: Sku::ActionsLinux,
                jobs: 10,
                minutes: 300,
                cost: Usd(1_800_000),
            },
            PricedRunner {
                sku: Sku::Linux8Core,
                jobs: 2,
                minutes: 40,
                cost: Usd(880_000),
            },
        ],
        free: 3,
        services: 1,
        unknown: 1,
        lost: 2,
        untimed: 1,
    }
}

pub(crate) fn savings() -> Savings {
    Savings {
        until: 1_761_400_000,
        month: saved_window(Period::Month),
        last_30_days: saved_window(Period::Last30Days),
    }
}

pub(crate) fn notify_test() -> NotifyTest {
    NotifyTest {
        targets: vec![Delivery {
            target: "phone".to_owned(),
            error: Some("connection refused".to_owned()),
        }],
    }
}

fn runner_failure() -> RunnerFailure {
    RunnerFailure {
        repository: "tidewater/halyard".to_owned(),
        profile: "halyard".to_owned(),
        error: "the image does not start".to_owned(),
        held_for: 300,
    }
}

/// What `[notify]` webhooks receive.
pub(crate) fn notification() -> Notification {
    Notification {
        event: NotifyEvent::Digest,
        title: "This week".to_owned(),
        message: "120 jobs finished".to_owned(),
        url: Some("http://127.0.0.1:7341/".to_owned()),
        priority: Priority::Default,
        jobs: vec![job_summary()],
        runners: vec![runner_failure()],
        insights: insights_of_every_kind(),
        digest: Some(digest()),
    }
}

/// A notification with nothing that serializing skips.
pub(crate) fn notification_empty() -> Notification {
    Notification {
        url: None,
        jobs: Vec::new(),
        runners: Vec::new(),
        insights: Vec::new(),
        digest: None,
        ..notification()
    }
}

pub(crate) fn notify_event() -> Vec<NotifyEvent> {
    let every = vec![
        NotifyEvent::Failed,
        NotifyEvent::Lost,
        NotifyEvent::Runner,
        NotifyEvent::Insights,
        NotifyEvent::Digest,
        NotifyEvent::Test,
    ];
    for event in &every {
        match event {
            NotifyEvent::Failed
            | NotifyEvent::Lost
            | NotifyEvent::Runner
            | NotifyEvent::Insights
            | NotifyEvent::Digest
            | NotifyEvent::Test => {}
        }
    }
    every
}

pub(crate) fn priority() -> Vec<Priority> {
    let every = vec![Priority::Low, Priority::Default, Priority::High];
    for priority in &every {
        match priority {
            Priority::Low | Priority::Default | Priority::High => {}
        }
    }
    every
}

/// One envelope of each event, as `/v1/events` streams them and the history stores the durable ones.
pub(crate) fn events() -> Vec<EventEnvelope> {
    let every = vec![
        Event::JobQueued { job: job_summary() },
        Event::JobStarted { job: job_summary() },
        Event::JobStep {
            job: 42,
            step: step(),
        },
        Event::JobFinished { job: job_summary() },
        Event::SlotChanged {
            profile: "halyard".to_owned(),
            slot: slot_status(),
        },
        Event::ProfileChanged {
            profile: profile_status(),
        },
        Event::ProfileRemoved {
            profile: "halyard".to_owned(),
        },
        Event::BudgetChanged { budget: budget() },
        Event::RepositoryChanged {
            repository: repository_status(),
        },
        Event::RepositoryRemoved {
            repository: "tidewater/halyard".to_owned(),
        },
        Event::CacheReseeded {
            profile: "halyard".to_owned(),
            output: "reseeded".to_owned(),
            error: Some("the seed container failed".to_owned()),
        },
        Event::RecordCreated { record: record() },
        Event::RecordDeleted {
            job: 42,
            removed_by: RecordRemoval::Age,
        },
        Event::SizingChanged {
            change: sizing_change(),
        },
        Event::RunnerFailed {
            failure: runner_failure(),
        },
        Event::JobSample {
            job: 42,
            sample: sample(),
        },
        Event::HostSample {
            sample: host_sample(),
        },
        Event::LogLine {
            job: 42,
            line: 7,
            text: "Compiling runnerd".to_owned(),
        },
    ];
    for event in &every {
        match event {
            Event::JobQueued { .. }
            | Event::JobStarted { .. }
            | Event::JobStep { .. }
            | Event::JobFinished { .. }
            | Event::SlotChanged { .. }
            | Event::ProfileChanged { .. }
            | Event::ProfileRemoved { .. }
            | Event::BudgetChanged { .. }
            | Event::RepositoryChanged { .. }
            | Event::RepositoryRemoved { .. }
            | Event::CacheReseeded { .. }
            | Event::RecordCreated { .. }
            | Event::RecordDeleted { .. }
            | Event::SizingChanged { .. }
            | Event::RunnerFailed { .. }
            | Event::JobSample { .. }
            | Event::HostSample { .. }
            | Event::LogLine { .. } => {}
        }
    }
    every
        .into_iter()
        .map(|event| EventEnvelope {
            id: Some(7),
            at: 1_761_400_000,
            event,
        })
        .collect()
}

pub(crate) fn pause_query() -> PauseQuery {
    PauseQuery { force: true }
}

pub(crate) fn pairing_query() -> PairingQuery {
    PairingQuery {
        origin: Some("https://ci.tidewater.example".to_owned()),
    }
}

pub(crate) fn replace_query() -> ReplaceQuery {
    ReplaceQuery {
        revision: Some("3f2a".to_owned()),
    }
}

pub(crate) fn job_query() -> JobQuery {
    JobQuery {
        repository: Some("tidewater/halyard".to_owned()),
        state: Some(JobState::Completed),
        conclusion: Some("failure".to_owned()),
        workflow: Some("ci".to_owned()),
        name: Some("build".to_owned()),
        since: Some(1_760_800_000),
        until: Some(1_761_400_000),
        before: Some(42),
        limit: Some(50),
    }
}

pub(crate) fn log_query() -> LogQuery {
    LogQuery {
        from: Some(100),
        follow: true,
    }
}

pub(crate) fn host_sample_query() -> HostSampleQuery {
    HostSampleQuery {
        since: Some(1_761_400_000),
    }
}

pub(crate) fn activity_query() -> ActivityQuery {
    ActivityQuery {
        since: Some(1_761_300_000),
    }
}

pub(crate) fn stats_query() -> StatsQuery {
    StatsQuery {
        repository: Some("tidewater/halyard".to_owned()),
        since: Some(1_760_800_000),
        until: Some(1_761_400_000),
        bucket: Bucket::Hour,
    }
}

pub(crate) fn sizing_query() -> SizingQuery {
    SizingQuery {
        since: Some(1_760_800_000),
    }
}

pub(crate) fn insights_query() -> InsightsQuery {
    InsightsQuery { days: Some(14) }
}

pub(crate) fn notify_test_query() -> NotifyTestQuery {
    NotifyTestQuery {
        target: Some("phone".to_owned()),
    }
}

pub(crate) fn cache_history_query() -> CacheHistoryQuery {
    CacheHistoryQuery {
        since: Some(1_761_300_000),
    }
}

pub(crate) fn event_query() -> EventQuery {
    EventQuery {
        job: Some(42),
        logs: true,
    }
}

pub(crate) fn remove_records_query() -> RemoveRecordsQuery {
    RemoveRecordsQuery {
        all: true,
        older_than: Some("7d".to_owned()),
    }
}

pub(crate) fn shell_query() -> ShellQuery {
    ShellQuery {
        step: Some(3),
        cols: Some(120),
        rows: Some(40),
    }
}
