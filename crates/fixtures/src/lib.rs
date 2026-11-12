//! Daemon data every client's parity test draws, and the facts `runnerd-core` computes from it, which the dashboard,
//! the TUI and `runnerctl` must each state as core words them (ADR 0062). The data is chosen where clients drifted
//! before: a lost, an elsewhere, a `startup_failure`, a timed-out and a cancelled job, one cancelled before any
//! runner took it, and one its runner failed (`runner fault`); an adopted job without a name;
//! a step without an end time and a cancelled one with times; a job no pool serves; a measured budget of which the
//! host's memory leaves less free than `MemoryMax` does, and a job waiting for the host; warm and starting runners
//! and a slot whose job is known before GitHub lists its runner busy; caps past the budget; records near
//! `max_total`; a build cache that refused uploads and had no lookups; and jobs priced at GitHub's rates on two
//! runners, with a 61-second and a 0-second job, and jobs of a public repository, of Dependabot, of a repository
//! GitHub has not answered for, lost, without an end and run elsewhere; auto sizing's plan, with a change next,
//! a profile a change holds and a `cpus` raise it cannot make; and insights of a job its CPU cap holds back, as a
//! warning and as a notice, beside the memory pressure one.
//! The job history's lenses have their own cases in `lens` (searches, the slots' timeline and comparisons) and logs
//! in `logs`.
//!
//! Test support only: client tests depend on it, a client never does, so no client depends on another to share it.
//! It computes nothing itself; every fact is core's.

use runnerd_core::api::{
    self, AdmissionPolicy, Budget, BuildCacheCounts, BuildCacheStats, CacheKind, CacheVolume,
    Caches, CpuBudget, Figure, HostBudget, HostMemory, Job, JobMetrics, JobRunner, JobState,
    JobSummary, LogSummary, OutcomeKind, PoolStatus, ProfileStatus, QueuedJob, Record,
    RecordLimits, Records, RepositoryStatus, RunnerStatus, SlotStatus, Status, Step, Timeline,
    Tone, WaitKind, Waiting,
};

/// When the clients draw the fixtures: 2026-10-22T14:03:07Z.
pub const NOW: u64 = 1_792_677_787;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// The job whose page every client draws: timed out, with a step without an end time, a cancelled step with times,
/// caps past the budget and a build cache that only stored and refused.
pub const TIMED_OUT: u64 = 9204;
/// A running job of halyard/ledger in jvm slot 1.
pub const RUNNING: u64 = 9101;
/// A running job of halyard/storefront whose runner in node slot 1 GitHub does not list busy yet.
pub const NOT_BUSY: u64 = 9102;
/// A job its runner was lost with.
pub const LOST: u64 = 9201;
/// A job another machine's runner ran.
pub const ELSEWHERE: u64 = 9202;
/// A job whose run failed before any runner took it.
pub const STARTUP_FAILURE: u64 = 9203;
/// A cancelled job.
pub const CANCELLED: u64 = 9205;
/// A job a newer push cancelled while it was queued, which no runner took.
pub const NEVER_STARTED: u64 = 9207;
/// A job a runner took before the daemon saw it queued, so the daemon never learnt its name.
pub const UNNAMED: u64 = 9206;
/// A job that concluded `failure` because its runner's Sysbox lost the seccomp listener, not because of its code.
pub const RUNNER_FAULT: u64 = 9208;
/// A queued job asking for a label no pool of its repository has.
pub const NO_POOL: u64 = 9301;
/// A queued job waiting for the host's memory, which leaves less than `MemoryMax` does.
pub const HOST_SHORT: u64 = 9302;
/// A queued job that fits, behind [`HOST_SHORT`], which waited long enough that no younger job starts before it.
pub const BEHIND_MEMORY: u64 = 9303;

mod lens;
mod logs;

pub use lens::{
    DeltaFact, EndFact, GapFact, LENS_BASELINE, LENS_COMPARED, LENS_FIRST_RUN, LENS_KEPT,
    LENS_ORPHAN, LENS_REMOVED, LENS_SINCE, LENS_SKIPPED, LENS_UNKNOWN_END, LENS_UNWATCHED,
    LENS_WATCHED_AGAIN, LENS_WILDCARD, LensFacts, LensHistory, LensSamples, MarkFact,
    lens_comparison, lens_facts, lens_history, lens_history_at, lens_observed, lens_records,
    lens_records_at, lens_timeline,
};
pub use logs::{
    LOG_EXPIRED, LOG_FAILED, LOG_MISSING, LOG_NOT_OUTLINED, LOG_RUNNING, LOG_TRUNCATED, LogFacts,
    LoggedJob, log_facts, logged_jobs,
};

const LEDGER: &str = "halyard/ledger";
const STOREFRONT: &str = "halyard/storefront";

/// What the daemon answers with.
#[derive(Debug, Clone)]
pub struct Daemon {
    /// `GET /v1/status`.
    pub status: Status,
    /// Every job, newest first: the running ones, then the finished.
    pub jobs: Vec<Job>,
    /// `GET /v1/records`.
    pub records: Records,
    /// `GET /v1/caches`.
    pub caches: Caches,
    /// `GET /v1/savings`.
    pub savings: api::Savings,
}

impl Daemon {
    /// The job with this ID.
    pub fn job(&self, id: u64) -> Option<&Job> {
        self.jobs.iter().find(|job| job.summary.id == id)
    }

    /// The jobs' summaries, newest first, as `GET /v1/jobs` lists them.
    pub fn summaries(&self) -> Vec<JobSummary> {
        self.jobs.iter().map(|job| job.summary.clone()).collect()
    }

    /// The finished jobs' summaries, newest first.
    pub fn finished(&self) -> Vec<JobSummary> {
        self.summaries()
            .into_iter()
            .filter(|job| job.state.is_final())
            .collect()
    }

    /// The latest runs of `job`'s workflow and name in its repository, oldest first, as a job's page reads them.
    pub fn runs(&self, job: u64) -> Vec<JobSummary> {
        let Some(job) = self.job(job) else {
            return Vec::new();
        };
        let mut runs: Vec<JobSummary> = self
            .summaries()
            .into_iter()
            .filter(|run| {
                run.repository == job.summary.repository
                    && run.workflow == job.summary.workflow
                    && run.name == job.summary.name
            })
            .collect();
        runs.reverse();
        runs
    }
}

fn runner(repository: &str, name: &str, busy: bool, warm: bool, reserved: u64) -> RunnerStatus {
    RunnerStatus {
        repository: repository.to_owned(),
        name: name.to_owned(),
        runner_id: 70,
        container: format!("c-{name}"),
        busy,
        started_at: NOW - 300,
        reserved,
        warm,
    }
}

fn pool(profile: &str, labels: &[&str]) -> PoolStatus {
    PoolStatus {
        profile: profile.to_owned(),
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        queued_jobs: 0,
    }
}

fn repository(name: &str, pool: PoolStatus, queued_jobs: usize) -> RepositoryStatus {
    RepositoryStatus {
        name: name.to_owned(),
        pools: vec![pool],
        paused: false,
        queued_jobs,
        listing: None,
    }
}

fn queued(id: u64, repository: &str, name: &str, ago: u64, waiting: Waiting) -> QueuedJob {
    QueuedJob {
        id,
        repository: repository.to_owned(),
        profile: None,
        run_id: id + 1000,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: name.to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        queued_at: NOW - ago,
        waiting: Some(waiting),
    }
}

/// A 64G host whose memory leaves the slice 22.5G: 18G its runners hold and 8.5G available, less a 4G margin.
fn host() -> HostBudget {
    HostBudget {
        memory: HostMemory {
            at: NOW - 2,
            total: 64 * GIB,
            available: 8 * GIB + 512 * MIB,
            slice: 18 * GIB,
            some: None,
            full: None,
            holders: Vec::new(),
            ceiling: None,
            swap: Some(api::Swap {
                total: 16 * GIB,
                free: 4 * GIB + 512 * MIB,
            }),
        },
        limit: 22 * GIB + 512 * MIB,
        spare: 512 * MIB,
        stalled: false,
        credit: None,
    }
}

/// A measured budget of whose 2G left the host leaves 512M; jvm's three
/// slots run, are warm and start a runner, node's first holds a runner whose job is known before GitHub lists it
/// busy.
fn status() -> Status {
    let jvm = |slot: usize, runner: Option<RunnerStatus>| SlotStatus { slot, runner };
    Status {
        version: "0.0.13".to_owned(),
        // `runnerd stop` waits for the running jobs.
        shutting_down: true,
        stopping: Some(api::Stopping::WaitsForJobs),
        github_limited_until: Some(NOW + 750),
        budget: Budget {
            slice: "runnerd.slice".to_owned(),
            memory_max: Some(24 * GIB),
            memory_in_use: 22 * GIB,
            pooled: Some(api::Pooled {
                credit: 256 * MIB,
                counted: 22 * GIB - 256 * MIB,
                runners: 3,
            }),
            // The growth count: what runs uses, and the room kept for it to grow.
            counted: Some(api::Counted {
                used: 15 * GIB,
                kept: Some(7 * GIB - 256 * MIB),
                jobs: 3,
                brake: None,
            }),
            admission: AdmissionPolicy::Measured,
            held: false,
            mirror_memory: 0,
            seed_memory: Vec::new(),
            shell_memory: Vec::new(),
            host: Some(host()),
            cpu: Some(CpuBudget {
                cpus: 16_000,
                budget: 16_000,
                in_use: 12_500,
                held: false,
            }),
            slice_cpus: None,
        },
        profiles: vec![
            ProfileStatus {
                name: "jvm".to_owned(),
                image: "runnerd-runner:jvm".to_owned(),
                cpus: 4,
                memory: 8 * GIB,
                reseeding: false,
                slots: vec![
                    jvm(
                        1,
                        Some(runner(LEDGER, "runnerd-ab12-jvm-1", true, false, 8 * GIB)),
                    ),
                    jvm(
                        2,
                        Some(runner(LEDGER, "runnerd-ab12-jvm-2", false, true, 6 * GIB)),
                    ),
                    jvm(
                        3,
                        Some(runner(LEDGER, "runnerd-ab12-jvm-3", false, false, 4 * GIB)),
                    ),
                ],
            },
            ProfileStatus {
                name: "node".to_owned(),
                image: "runnerd-runner:node".to_owned(),
                cpus: 2,
                memory: 4 * GIB,
                reseeding: false,
                slots: vec![
                    SlotStatus {
                        slot: 1,
                        runner: Some(runner(
                            STOREFRONT,
                            "runnerd-ab12-node-1",
                            false,
                            false,
                            4 * GIB,
                        )),
                    },
                    SlotStatus {
                        slot: 2,
                        runner: None,
                    },
                ],
            },
        ],
        repositories: vec![
            repository(LEDGER, pool("jvm", &["self-hosted", "jvm"]), 1),
            repository(STOREFRONT, pool("node", &["self-hosted", "node"]), 1),
        ],
        queued: queued_jobs(),
    }
}

/// The queue: a job no pool serves, one the host is short for, and one behind it.
fn queued_jobs() -> Vec<QueuedJob> {
    vec![
        queued(
            NO_POOL,
            LEDGER,
            "render",
            125,
            Waiting::NoPool {
                labels: vec!["self-hosted".to_owned(), "gpu".to_owned()],
            },
        ),
        queued(
            HOST_SHORT,
            STOREFRONT,
            "lint",
            47,
            Waiting::HostMemory {
                needs: GIB,
                free: 512 * MIB,
            },
        ),
        queued(
            BEHIND_MEMORY,
            LEDGER,
            "compile",
            12,
            Waiting::BehindMemory { job: HOST_SHORT },
        ),
    ]
}

/// A job of `repository`'s `CI / test` that has not started, with no times yet.
fn summary(id: u64, repository: &str, state: JobState, conclusion: Option<&str>) -> JobSummary {
    JobSummary {
        id,
        repository: repository.to_owned(),
        run_id: id + 1000,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: "test".to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        labels: vec!["self-hosted".to_owned(), "jvm".to_owned()],
        state,
        conclusion: conclusion.map(str::to_owned),
        runner: None,
        timeline: Timeline::default(),
        metrics: JobMetrics::default(),
        log: LogSummary::default(),
        runner_fault: false,
        record_mark: None,
    }
}

fn job_runner(name: &str, profile: &str, slot: usize) -> JobRunner {
    JobRunner {
        name: name.to_owned(),
        profile: profile.to_owned(),
        slot,
        memory_cap: Some(8 * GIB),
        cpu_cap: Some(4000),
    }
}

/// A job that ran from `started` to `finished` seconds before now, queued 20 seconds before it started and seen
/// by runnerd 3 seconds after GitHub queued it.
fn ran(mut summary: JobSummary, started: u64, finished: Option<u64>) -> JobSummary {
    summary.runner = Some(job_runner("runnerd-ab12-jvm-1", "jvm", 1));
    summary.timeline = Timeline {
        queued_at: NOW - started - 20,
        container_started_at: Some(NOW - started - 12),
        started_at: Some(NOW - started),
        finished_at: finished.map(|finished| NOW - finished),
        cleaned_at: finished.map(|finished| NOW - finished + 4),
        unseen: 0,
        seen_at: Some(NOW - started - 17),
    };
    summary
}

fn step(number: u32, name: &str, conclusion: Option<&str>, from: u64, to: Option<u64>) -> Step {
    Step {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: conclusion.map(str::to_owned),
        started_at: Some(NOW - from),
        completed_at: to.map(|to| NOW - to),
    }
}

fn job(summary: JobSummary) -> Job {
    Job {
        summary,
        steps: Vec::new(),
        record: None,
        admission: None,
    }
}

/// Jobs no runner of this machine ran, newest first: one whose run failed before any runner took it, one a newer
/// push cancelled while queued, and one another machine's runner ran.
fn not_run_here() -> [JobSummary; 3] {
    let mut startup_failure = summary(
        STARTUP_FAILURE,
        LEDGER,
        JobState::Completed,
        Some("startup_failure"),
    );
    startup_failure.timeline = Timeline {
        queued_at: NOW - 9000,
        finished_at: Some(NOW - 8990),
        ..Timeline::default()
    };
    let mut elsewhere = summary(ELSEWHERE, LEDGER, JobState::Elsewhere, Some("success"));
    elsewhere.timeline = Timeline {
        queued_at: NOW - 9500,
        started_at: Some(NOW - 9480),
        finished_at: Some(NOW - 9100),
        ..Timeline::default()
    };
    let mut never_started = summary(
        NEVER_STARTED,
        LEDGER,
        JobState::Elsewhere,
        Some("cancelled"),
    );
    never_started.timeline = Timeline {
        queued_at: NOW - 9450,
        finished_at: Some(NOW - 9440),
        ..Timeline::default()
    };
    [startup_failure, never_started, elsewhere]
}

fn jobs() -> Vec<Job> {
    let [startup_failure, never_started, elsewhere] = not_run_here();
    let mut running = ran(summary(RUNNING, LEDGER, JobState::Running, None), 240, None);
    "build".clone_into(&mut running.name);
    let mut not_busy = ran(
        summary(NOT_BUSY, STOREFRONT, JobState::Running, None),
        30,
        None,
    );
    "e2e".clone_into(&mut not_busy.name);
    not_busy.runner = Some(JobRunner {
        memory_cap: Some(4 * GIB),
        cpu_cap: Some(2000),
        ..job_runner("runnerd-ab12-node-1", "node", 1)
    });

    let mut unnamed = ran(
        summary(UNNAMED, STOREFRONT, JobState::Completed, Some("success")),
        900,
        Some(780),
    );
    unnamed.workflow = None;
    unnamed.name = String::new();
    let cancelled = ran(
        summary(CANCELLED, LEDGER, JobState::Completed, Some("cancelled")),
        1800,
        Some(1700),
    );
    let mut runner_fault = ran(
        summary(
            RUNNER_FAULT,
            STOREFRONT,
            JobState::Completed,
            Some("failure"),
        ),
        1500,
        Some(1400),
    );
    "image".clone_into(&mut runner_fault.name);
    runner_fault.runner_fault = true;
    let mut timed_out = ran(
        summary(TIMED_OUT, LEDGER, JobState::Completed, Some("timed_out")),
        7180,
        Some(3500),
    );
    // Queued while runnerd was not running, for 15 s of its 20 s wait.
    timed_out.timeline.unseen = 15;
    // Caps past the whole budget, and a CPU cap only a tenth rounded up writes.
    timed_out.runner = Some(JobRunner {
        memory_cap: Some(40 * GIB),
        cpu_cap: Some(1250),
        ..job_runner("runnerd-ab12-jvm-1", "jvm", 1)
    });
    timed_out.metrics.memory_peak = Some(5 * GIB + 300 * MIB);
    timed_out.metrics.cpu_time = Some(4600);
    // Its 1h 01m 20s run: throttled in 3,600 of its 20,000 busy periods, and waiting for a CPU 1,104 s.
    timed_out.metrics.cpu_periods = Some(20_000);
    timed_out.metrics.cpu_throttled = Some(3_600);
    timed_out.metrics.cpu_stalled = Some(1_104_000);
    timed_out.metrics.build_cache = BuildCacheCounts {
        hits: 0,
        misses: 0,
        read_bytes: 0,
        written_bytes: 512 * MIB,
        refused: 3,
        writer: true,
    };
    let lost = ran(
        summary(LOST, LEDGER, JobState::Lost, None),
        10_000,
        Some(9600),
    );
    let succeeded = ran(
        summary(9200, LEDGER, JobState::Completed, Some("success")),
        12_000,
        Some(11_500),
    );

    vec![
        job(running),
        job(not_busy),
        job(unnamed),
        job(cancelled),
        job(runner_fault),
        Job {
            steps: vec![
                step(1, "Set up job", Some("success"), 7180, Some(7177)),
                step(2, "Run tests", Some("timed_out"), 7177, Some(3577)),
                // Completed, but GitHub gave no completion time.
                step(3, "Upload report", Some("success"), 3577, None),
                step(4, "Post cleanup", Some("cancelled"), 3547, Some(3500)),
            ],
            ..job(timed_out)
        },
        job(startup_failure),
        job(never_started),
        job(elsewhere),
        job(lost),
        job(succeeded),
    ]
}

/// Two records taking 1.9G of a 2G `max_total`.
fn records() -> Records {
    let record = |job: u64, bytes: u64| Record {
        job,
        repository: LEDGER.to_owned(),
        profile: "jvm".to_owned(),
        workflow: Some("CI".to_owned()),
        name: Some("test".to_owned()),
        created_at: NOW - 3400,
        bytes,
        image: format!("runnerd-record:{job}"),
    };
    Records {
        records: vec![record(TIMED_OUT, GIB), record(LOST, 900 * MIB)],
        limits: RecordLimits {
            max_total: 2 * GIB,
            max_count: 10,
            max_age: 7 * 86_400,
            min_free: 10 * GIB,
        },
        shells: Vec::new(),
    }
}

/// The build cache's volume and a week in which it only stored and refused uploads.
fn caches() -> Caches {
    Caches {
        measured_at: Some(NOW - 60),
        volumes: vec![CacheVolume {
            name: "runnerd-build-cache".to_owned(),
            kind: CacheKind::Build,
            bytes: 3 * GIB,
            limit: Some(10 * GIB),
        }],
        build: Some(BuildCacheStats {
            bytes: 3 * GIB,
            entries: 120,
            limit: 10 * GIB,
            week: BuildCacheCounts {
                hits: 0,
                misses: 0,
                read_bytes: 0,
                written_bytes: 2 * GIB,
                refused: 4,
                writer: true,
            },
            actions_cache: true,
        }),
    }
}

/// Auto sizing's plan at `at` (s): its next look lowers node's memory and cannot raise node's `cpus`, which its tests
/// sit at, for want of CPU budget. Every client shows both: the change, and `api::AutoPlan::refusal`.
pub fn auto_plan(at: u64) -> api::AutoPlan {
    api::AutoPlan {
        every: 300,
        interval: 86_400,
        raise_interval: 3600,
        max_step: 25,
        next: vec![api::SizingChange {
            at,
            profile: "node".to_owned(),
            from: api::Resize {
                memory: 7 * GIB,
                slots: 2,
                cpus: None,
            },
            to: api::Resize {
                memory: 5376 * MIB,
                slots: 2,
                cpus: None,
            },
            reason: "lower memory".to_owned(),
            uncertain: false,
        }],
        held: Vec::new(),
        blocked: None,
        refused: vec![api::SizingRefused {
            profile: "node".to_owned(),
            reason: format!(
                "3 runs of {STOREFRONT} CI / test were held back by its 8 CPUs, throttled in 15% of their run or \
                 more: under measured admission they bound no slot, so the cap only slows them; 9 CPUs do not fit \
                 the CPU budget with every profile's warm runners and one more runner at the largest cap"
            ),
        }],
        refused_slots: vec![api::SizingRefused {
            profile: "node".to_owned(),
            reason: "14 jobs since its last change waited 240s for a slot (p95) while memory and CPU had room, 5 at \
                     once at most; at 3 slots, beside the other profiles' 6, each slot would keep 1G of Docker's \
                     disk, down from 2G, under the 2G auto sizing leaves each slot"
                .to_owned(),
        }],
        waited_before_change: Vec::new(),
    }
}

/// `GET /v1/sizing` at `at` (s) under auto sizing, with [`auto_plan`]: jvm keeps its 9G, and a change 2 hours before
/// holds it from any other for 1 more hour, its jobs since having waited for no slot; node's next look would lower
/// its memory, and cannot raise its `cpus`; android's jobs need 17G, but `[host] memory_ceiling` holds it at its 14G;
/// gradle's need 13.25G, but its `max_memory` holds it at its 11G; and the config file changed on disk, so no look
/// changes anything until runnerd runs with it.
pub fn sizing(at: u64) -> api::Sizing {
    let spread = |p50, p95, p99, max| api::Spread { p50, p95, p99, max };
    let profile = |name: &str, memory: u64, peak: u64, advice| api::ProfileSizing {
        name: name.to_owned(),
        memory,
        cpus: 8,
        slots: 2,
        peaks: api::Peaks {
            jobs: 40,
            at_cap: 0,
            on_cap: 0,
            memory: spread(peak / 2, peak, peak, peak),
            cpu: spread(2000, 6000, 7600, 7600),
            cpu_runs: 40,
        },
        wait: spread(20, 300, 400, 400),
        slot_wait: spread(10, 240, 300, 300),
        starts: api::Starts::default(),
        busy: 70,
        wants_slots: true,
        advice,
        revert: None,
        hold: None,
        jobs: Vec::new(),
    };
    let mut plan = auto_plan(at);
    plan.blocked = Some(
        "the file changed on disk since runnerd read it; nothing is changed until runnerd runs with it (restart it, \
         or make the edit in the dashboard's editor)"
            .to_owned(),
    );
    plan.waited_before_change.push("jvm".to_owned());
    plan.held.push(api::SizingHold {
        profile: "jvm".to_owned(),
        changed: at - 7200,
        until: at + 3600,
        lower_until: at + 3600,
    });
    api::Sizing {
        since: at - 30 * 86_400,
        until: at,
        margin: 15,
        sample_interval: 2,
        min_jobs: 10,
        memory_max: Some(33 * GIB),
        admission: AdmissionPolicy::Measured,
        profiles: vec![
            profile(
                "jvm",
                9 * GIB,
                7800 * MIB,
                api::Advice::Keep { memory: 9 * GIB },
            ),
            profile(
                "node",
                7 * GIB,
                2150 * MIB,
                api::Advice::Lower {
                    memory: 3328 * MIB,
                    slots: 4,
                },
            ),
            api::ProfileSizing {
                hold: Some(api::MemoryHold {
                    need: 17 * GIB,
                    bound: 14 * GIB,
                    by: api::MemoryBound::Ceiling {
                        ceiling: 14 * GIB + 512 * MIB,
                        mirror: 512 * MIB,
                    },
                }),
                ..profile(
                    "android",
                    14 * GIB,
                    14 * GIB,
                    api::Advice::SliceTooSmall { memory: 17 * GIB },
                )
            },
            api::ProfileSizing {
                hold: Some(api::MemoryHold {
                    need: 13568 * MIB,
                    bound: 11 * GIB,
                    by: api::MemoryBound::MaxMemory,
                }),
                ..profile(
                    "gradle",
                    11 * GIB,
                    11 * GIB,
                    api::Advice::SliceTooSmall {
                        memory: 13568 * MIB,
                    },
                )
            },
        ],
        auto: true,
        changes: Vec::new(),
        auto_plan: Some(plan),
    }
}

/// The host's memory at `at` (s) while java and claude hold much of what is not available.
fn held_host(at: u64) -> HostMemory {
    HostMemory {
        at,
        total: 64 * GIB,
        available: 8 * GIB + 512 * MIB,
        slice: 18 * GIB,
        some: None,
        full: None,
        holders: vec![
            api::Holder {
                name: "java".to_owned(),
                memory: 9 * GIB + 307 * MIB,
            },
            api::Holder {
                name: "claude".to_owned(),
                memory: 5 * GIB + 102 * MIB,
            },
        ],
        ceiling: None,
        swap: None,
    }
}

/// The warning for today's 34 s under a tenth of the host on a host credit, as core makes it (`insights::insights`).
fn host_low(at: u64) -> api::Insight {
    let today = at / 86_400 * 86_400;
    let lowest = 4 * GIB + 102 * MIB;
    api::Insight {
        key: format!("host-low:{}", today / 86_400),
        severity: api::Severity::Warning,
        repository: String::new(),
        workflow: None,
        job: None,
        sentence: api::Insight::host_low_sentence(today, 34, lowest, 6 * GIB + 410 * MIB),
        kind: api::InsightKind::HostLow {
            day: today,
            seconds: 34,
            lowest,
        },
        runs: Vec::new(),
    }
}

/// What the host's memory came to on the two days [`insights`] lists: today on a host credit, under a tenth of the
/// host for 34 s; yesterday on none.
fn host_days(at: u64) -> Vec<api::HostHazard> {
    let today = at / 86_400 * 86_400;
    vec![
        // Today so far: CI ran on a host credit, and the host fell under a tenth of its memory for 34 s of it.
        api::HostHazard {
            since: today,
            until: at,
            running: 33_120,
            credited: 22_200,
            low_line: 20 * GIB,
            critical_line: 6 * GIB + 410 * MIB,
            low: api::HostSpan {
                seconds: 3840,
                credited: 2460,
            },
            critical: api::HostSpan {
                seconds: 34,
                credited: 34,
            },
            lowest: 4 * GIB + 102 * MIB,
            lowest_credited: Some(4 * GIB + 102 * MIB),
            lowest_swap_free: Some(8 * GIB + 205 * MIB),
        },
        // Yesterday, no credit stood and the host kept above both lines.
        api::HostHazard {
            since: today - 86_400,
            until: today,
            running: 7200,
            credited: 0,
            low_line: 20 * GIB,
            critical_line: 6 * GIB + 410 * MIB,
            low: api::HostSpan::default(),
            critical: api::HostSpan::default(),
            lowest: 31 * GIB,
            lowest_credited: None,
            lowest_swap_free: None,
        },
    ]
}

/// `GET /v1/insights` at `at` (s) under measured admission and auto sizing: ledger's tests held back by jvm's CPU cap
/// after a raise that did not help them, a warning; storefront's held back by node's, which auto sizing raises, a
/// notice; memory pressure that slowed storefront's downloads; and jobs held 42 minutes for the host's memory by
/// java and claude; and today, the host under a tenth of its memory for 34 s while a host credit stood, with what its
/// memory came to today and yesterday. The sentences are core's (`insights::insights`); the host-held and host-low ones
/// are worded by core's own functions from their figures.
pub fn insights(at: u64) -> api::Insights {
    let link = |job: u64, back: u64| api::RunLink {
        job,
        run_id: 9_400_000 + job,
        run_attempt: 1,
        branch: Some("main".to_owned()),
        url: None,
        at: at - back,
    };
    let insight =
        |key: &str, severity, repository: &str, job: Option<&str>, sentence: &str, kind| {
            api::Insight {
                key: key.to_owned(),
                severity,
                repository: repository.to_owned(),
                workflow: job.map(|_| "CI".to_owned()),
                job: job.map(str::to_owned),
                sentence: sentence.to_owned(),
                kind,
                runs: vec![link(TIMED_OUT, 3600)],
            }
        };
    api::Insights {
        since: at - 14 * 86_400,
        until: at,
        host: host_days(at),
        insights: vec![
            host_low(at),
            insight(
                "host-held:2987",
                api::Severity::Warning,
                LEDGER,
                None,
                &api::Insight::host_held_sentence(
                    3,
                    2530,
                    "halyard/ledger: CI / test",
                    &held_host(at),
                ),
                api::InsightKind::HostHeld {
                    jobs: 3,
                    waited: 2530,
                },
            ),
            insight(
                "throttled:halyard/storefront",
                api::Severity::Warning,
                STOREFRONT,
                None,
                "halyard/storefront: memory pressure slowed the downloads of 2 of its 31 jobs: the host, or a job at \
                 its cap, was short of memory.",
                api::InsightKind::Throttled {
                    throttled: 2,
                    total: 31,
                },
            ),
            insight(
                "cpu-held:halyard/ledger/CI/test",
                api::Severity::Warning,
                LEDGER,
                Some("test"),
                "halyard/ledger: CI / test was held back by jvm's 8 CPUs in 4 of its last 10 runs, its quota \
                 throttling it in 15% of its run or more. The raise of jvm from 6 to 8 CPUs did not make it faster \
                 or less throttled, as when its tools start a worker per CPU, so its runs ask for no further raise; \
                 runnerctl sizing says what auto sizing does next.",
                api::InsightKind::CpuHeld {
                    profile: "jvm".to_owned(),
                    cpus: 8,
                    held: 4,
                    total: 10,
                    raised_from: Some(6),
                    set: None,
                },
            ),
            insight(
                "cpu-held:halyard/storefront/CI/test",
                api::Severity::Notice,
                STOREFRONT,
                Some("test"),
                "halyard/storefront: CI / test was held back by node's 8 CPUs in 3 of its last 9 runs, its quota \
                 throttling it in 15% of its run or more. Auto sizing raises node's cpus while each raise makes it \
                 faster or less throttled and the slice has room; runnerctl sizing says when it cannot.",
                api::InsightKind::CpuHeld {
                    profile: "node".to_owned(),
                    cpus: 8,
                    held: 3,
                    total: 9,
                    raised_from: None,
                    set: None,
                },
            ),
        ],
        waits: Vec::new(),
    }
}

/// The fixtures.
pub fn daemon() -> Daemon {
    Daemon {
        status: status(),
        jobs: jobs(),
        records: records(),
        caches: caches(),
        savings: savings(),
    }
}

/// The fixtures under a credit large enough that the host's limit, 24.5G, passes the 24G `MemoryMax` and still holds
/// room back: it reads the plain 22G, which leaves 2.5G, where the 20G counted leave 4G.
pub fn past_memory_max() -> Daemon {
    let mut daemon = daemon();
    let budget = &mut daemon.status.budget;
    budget.pooled = Some(api::Pooled {
        credit: 2 * GIB,
        counted: 20 * GIB,
        runners: 3,
    });
    let mut host = host();
    host.memory.available = 10 * GIB + 512 * MIB;
    host.limit = 24 * GIB + 512 * MIB;
    host.spare = 2 * GIB + 512 * MIB;
    budget.host = Some(host);
    daemon
}

/// The fixtures with a host credit standing (ADR 0108): of the 22G the runners reserve, the host counts 21.75G, the
/// 15G they use and 6.75G kept for growth, which leaves 768M of the 22.5G limit; the job [`HOST_SHORT`] still waits
/// for 1G.
pub fn host_credit() -> Daemon {
    let mut daemon = daemon();
    let status = &mut daemon.status;
    let mut host = host();
    host.credit = Some(api::HostCredit {
        bytes: 256 * MIB,
        used: 15 * GIB,
    });
    host.spare = 768 * MIB;
    status.budget.host = Some(host);
    for queued in &mut status.queued {
        if let Some(Waiting::HostMemory { free, .. }) = &mut queued.waiting {
            *free = 768 * MIB;
        }
    }
    daemon
}

/// A job of the pricing history that ran `seconds` and ended `ago` seconds before [`NOW`].
fn priced(repository: &str, profile: &str, seconds: u64, ago: u64) -> api::PricedRun {
    let finished = (NOW - ago) * 1000;
    api::PricedRun {
        repository: repository.to_owned(),
        workflow: Some("CI".to_owned()),
        labels: vec!["self-hosted".to_owned()],
        state: JobState::Completed,
        profile: Some(profile.to_owned()),
        queued_at: finished - seconds * 1000 - 20_000,
        started_at: Some(finished - seconds * 1000),
        finished_at: Some(finished),
    }
}

/// What the history's jobs would have cost: ledger's jvm jobs at `linux_8_core`, storefront's node jobs at the
/// standard runner, and docs, a public repository.
fn savings() -> api::Savings {
    let mut dependabot = priced(STOREFRONT, "node", 120, 600);
    dependabot.workflow = Some("Dependabot Updates".to_owned());
    let mut lost = priced(LEDGER, "jvm", 7200, 700);
    lost.state = JobState::Lost;
    let mut untimed = priced(STOREFRONT, "node", 60, 800);
    untimed.finished_at = None;
    let mut elsewhere = priced(STOREFRONT, "node", 60, 900);
    elsewhere.state = JobState::Elsewhere;
    let history = api::PricingHistory {
        runs: vec![
            priced(LEDGER, "jvm", 1260, 3600),
            priced(LEDGER, "jvm", 61, 3000),
            priced(STOREFRONT, "node", 0, 2400),
            priced(STOREFRONT, "node", 600, 25 * 86_400),
            priced("halyard/docs", "node", 300, 1800),
            priced("halyard/docs", "jvm", 300, 1200),
            priced("halyard/archive", "node", 300, 1000),
            dependabot,
            lost,
            untimed,
            elsewhere,
        ],
        visibility: [
            (LEDGER, api::Visibility::Private),
            (STOREFRONT, api::Visibility::Private),
            ("halyard/docs", api::Visibility::Public),
        ]
        .into_iter()
        .map(|(name, visibility)| (name.to_owned(), visibility))
        .collect(),
        kept_since: (NOW - 60 * 86_400) * 1000,
    };
    let pricing = api::Pricing {
        profiles: [("jvm".to_owned(), api::Sku::Linux8Core)]
            .into_iter()
            .collect(),
        default_sku: api::Sku::ActionsLinux,
    };
    api::Savings::of(&history, &pricing, NOW * 1000)
}

/// A finished job as every job list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    /// The job.
    pub job: u64,
    /// Its name, as every client writes it.
    pub title: String,
    /// Its name without its workflow, for a client that sets the workflow apart: `job 9206` for one without.
    pub name: String,
    /// Its outcome, such as `timed out`.
    pub label: String,
    /// What its outcome means, for a client's colour.
    pub kind: OutcomeKind,
    /// How long it waited for a runner; `None` for a job no client gives a wait.
    pub waited: Option<String>,
    /// How long it ran; `None` for a job no client gives a run time.
    pub ran: Option<String>,
}

/// A step as core says it: its outcome and how long it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFact {
    /// Its name.
    pub name: String,
    /// Such as `cancelled`.
    pub label: String,
    /// What its outcome means.
    pub kind: OutcomeKind,
    /// How long it took; `None` for a step no client gives a duration.
    pub took: Option<String>,
}

/// Why a queued job waits, as core words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitFact {
    /// The queued job.
    pub job: u64,
    /// Such as `No profile serves the labels self-hosted, gpu`.
    pub reason: String,
    /// What it waits on, for a client's colour.
    pub kind: WaitKind,
}

/// A slot's state as core says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotFact {
    /// The slot's profile.
    pub profile: String,
    /// 1-based.
    pub slot: usize,
    /// One word, as a table column says it: `warm`.
    pub word: &'static str,
    /// As a sentence's tail: `a warm runner for halyard/ledger, waiting for a job`.
    pub line: String,
    /// What it means at a glance.
    pub tone: Tone,
}

/// The facts of [`TIMED_OUT`]'s page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFacts {
    /// Its row: its name and outcome.
    pub row: JobRow,
    /// Its steps in order.
    pub steps: Vec<StepFact>,
    /// What its container was held to: `capped at 40G and 1.3 CPUs`.
    pub limits: String,
    /// What it did with the build cache, part by part: lookups, served, stored, refused.
    pub build_cache: Vec<String>,
    /// The uploads the build cache refused it, the part a client with room for one shows.
    pub build_cache_refused: String,
    /// What it used over its life, as core words each figure.
    pub totals: Vec<api::Total>,
    /// Its CPU time.
    pub cpu_time: String,
    /// What held its CPU back: its cap's throttling and its waits for a CPU.
    pub cpu_held: String,
    /// What of its wait passed while runnerd was not running.
    pub unseen: String,
    /// The labels of its milestones in order, `Seen by runnerd` among them.
    pub milestones: Vec<&'static str>,
    /// Of its latest runs that started and ended, how many failed.
    pub failed: usize,
    /// How many of its latest runs started and ended.
    pub finished: usize,
}

/// What every client must say of [`daemon`], each as far as it shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// Each finished job, newest first.
    pub rows: Vec<JobRow>,
    /// What is reserved of the memory budget: `22G of 24G reserved in runnerd.slice by predicted peaks`.
    pub budget: String,
    /// The CPU budget's clause.
    pub cpu: String,
    /// The host's clause, with what is free while it leaves less than `MemoryMax`: `host: …; leaves the slice
    /// 22.5G of its 24G, so 512M is free`.
    pub host: String,
    /// The bar's free part: what the host leaves free.
    pub free: String,
    /// The bar's part the host holds back.
    pub host_part: String,
    /// The health figures of the slots and the queue, worded by core: what runs, starts, is warm and waits.
    pub figures: Vec<Figure>,
    /// Why each queued job waits, oldest first.
    pub waits: Vec<WaitFact>,
    /// Each slot that holds a runner.
    pub slots: Vec<SlotFact>,
    /// [`TIMED_OUT`]'s page.
    pub job: JobFacts,
    /// How many records there are and what they take of their limits.
    pub records_usage: String,
    /// What being near `max_total` means.
    pub records_pressure: String,
    /// The records' summary's tone.
    pub records_tone: Tone,
    /// The build cache's week.
    pub build_cache_week: String,
    /// Where jobs' GitHub Actions cache is.
    pub actions_cache: String,
    /// The month's job minutes priced at GitHub's rates, with their basis.
    pub savings: String,
    /// The last 30 days', which reach the jobs of September.
    pub savings_30_days: String,
    /// How the daemon stops.
    pub stopping: &'static str,
    /// Why no runner starts while GitHub's rate limit is used up.
    pub github: &'static str,
}

fn row(job: &JobSummary) -> JobRow {
    let outcome = job.outcome();
    JobRow {
        job: job.id,
        title: job.title().to_string(),
        name: job.title().name.into_owned(),
        label: outcome.label.into_owned(),
        kind: outcome.kind,
        waited: job.waited(NOW).map(api::duration),
        ran: job.ran(NOW).map(api::duration),
    }
}

/// The build cache's stats of `daemon`.
#[expect(
    clippy::expect_used,
    reason = "test support: fixtures without their own jobs fail the test"
)]
fn build_stats(daemon: &Daemon) -> &BuildCacheStats {
    daemon
        .caches
        .build
        .as_ref()
        .expect("the build cache's stats")
}

/// The facts of `daemon`, as core computes them at [`NOW`].
///
/// # Panics
///
/// When the fixtures lack what they are made with, which fails the test that asked.
#[expect(
    clippy::expect_used,
    reason = "test support: fixtures without their own jobs fail the test"
)]
pub fn facts(daemon: &Daemon) -> Facts {
    let status = &daemon.status;
    let job = daemon.job(TIMED_OUT).expect("the timed-out job");
    let compared = job.compare(&daemon.runs(TIMED_OUT), NOW);
    let build = build_stats(daemon);
    let build_cache = job
        .summary
        .metrics
        .build_cache
        .activity()
        .expect("the build cache stored and refused");
    let build_cache_refused = build_cache.refused_line().expect("refused uploads");
    let parts = status.budget_parts();
    let stopping = status.stopping_line().expect("a stop");
    let github = status.github_line().expect("a used-up rate limit");
    Facts {
        rows: daemon.finished().iter().map(row).collect(),
        budget: status.budget.memory_reading().line(),
        cpu: status.budget.cpu.expect("a CPU budget").line(),
        host: status.budget.host_line().expect("a host reading"),
        free: parts.free.expect("room past the runners").reading.line(),
        host_part: parts
            .closed
            .expect("the host holds room back")
            .reading
            .line(),
        figures: status.health(NOW).figures(None),
        waits: status
            .queued
            .iter()
            .filter_map(|queued| {
                let waiting = queued.waiting.as_ref()?;
                Some(WaitFact {
                    job: queued.id,
                    reason: waiting.reason(queued, status, NOW),
                    kind: waiting.kind(),
                })
            })
            .collect(),
        slots: slot_facts(status),
        job: JobFacts {
            row: row(&job.summary),
            steps: job
                .steps
                .iter()
                .map(|step| {
                    let outcome = step.outcome();
                    StepFact {
                        name: step.name.clone(),
                        label: outcome.label.into_owned(),
                        kind: outcome.kind,
                        took: step.took(NOW).map(api::duration),
                    }
                })
                .collect(),
            limits: job.limits(None).line().expect("the job's caps"),
            build_cache: [
                build_cache.lookups_line(),
                build_cache.served_line(),
                build_cache.stored_line(),
                build_cache_refused.clone(),
            ]
            .into(),
            build_cache_refused,
            totals: job.summary.metrics.totals(),
            cpu_time: api::cpu_time(job.summary.metrics.cpu_time.expect("its CPU time")),
            cpu_held: job
                .summary
                .cpu_held(NOW)
                .expect("its CPU throttled and waiting"),
            unseen: job.summary.unseen_note(NOW).expect("downtime in its wait"),
            milestones: job
                .summary
                .timeline
                .milestones()
                .iter()
                .map(|milestone| milestone.label)
                .collect(),
            failed: compared.failed,
            finished: compared.finished,
        },
        records_usage: daemon.records.usage(),
        records_pressure: daemon
            .records
            .pressure()
            .expect("the records are near max_total")
            .to_owned(),
        records_tone: daemon.records.summary_tone(),
        build_cache_week: build.week_line(),
        actions_cache: build.actions_cache_line(),
        savings: daemon.savings.month.headline(),
        savings_30_days: daemon.savings.last_30_days.headline(),
        stopping,
        github,
    }
}

/// Each slot with a runner, as `Facts::slots` lists them.
fn slot_facts(status: &Status) -> Vec<SlotFact> {
    status
        .profiles
        .iter()
        .flat_map(|profile| {
            profile
                .slots
                .iter()
                .filter(|slot| slot.runner.is_some())
                .map(move |slot| {
                    let state = Status::slot_state(profile, slot);
                    SlotFact {
                        profile: profile.name.clone(),
                        slot: slot.slot,
                        word: state.word(),
                        line: state.line(),
                        tone: state.tone(),
                    }
                })
        })
        .collect()
}

/// The doctor's run 3 minutes before [`NOW`] for the config the daemon runs with (ADR 0132): sysbox fails, the slice
/// warns, two checks of one name (`repository`) warn, Docker passes and kvm is a note.
pub fn doctor() -> api::Doctor {
    let check = |name: &str, status, detail: &str, fix: Option<&str>| api::Check {
        name: name.to_owned(),
        status,
        detail: detail.to_owned(),
        fix: fix.map(str::to_owned),
    };
    api::Doctor {
        checks: vec![
            check("Docker", api::CheckStatus::Pass, "29.1.3, reachable", None),
            check(
                "sysbox",
                api::CheckStatus::Fail,
                "sysbox-runc is not a Docker runtime",
                Some("sudo systemctl restart sysbox docker"),
            ),
            check(
                "slice",
                api::CheckStatus::Warn,
                "runnerd.slice has MemoryMax=24G but no MemoryHigh",
                Some("systemctl --user edit runnerd.slice, then add MemoryHigh=22G under [Slice]"),
            ),
            check(
                "repository",
                api::CheckStatus::Warn,
                "halyard/docs is public: anyone who opens a pull request on it can run code on this host",
                Some("serve private repositories only"),
            ),
            check(
                "repository",
                api::CheckStatus::Warn,
                "halyard/archive is public: anyone who opens a pull request on it can run code on this host",
                Some("serve private repositories only"),
            ),
            check("kvm", api::CheckStatus::Info, "/dev/kvm is there", None),
        ],
        state: Some(api::DoctorState::Ready { at: NOW - 180 }),
    }
}

/// [`doctor`] in each state that is not current: never run since the start, failed on its deadline, stale for a
/// config replaced since, and a state from a newer daemon. None of them may read as all clear.
pub fn doctor_not_current() -> Vec<api::Doctor> {
    [
        api::DoctorState::NotRun,
        api::DoctorState::Failed {
            at: NOW - 30,
            error: "it took longer than 2m 00s".to_owned(),
        },
        api::DoctorState::Stale { at: NOW - 7200 },
        api::DoctorState::Unknown,
    ]
    .into_iter()
    .map(|state| api::Doctor {
        state: Some(state),
        ..doctor()
    })
    .collect()
}

/// The fixtures with GitHub failing (ADR 0132): ledger's listing answered 404 for the last 10 minutes and
/// storefront's gets no answer, beside the used-up rate limit.
pub fn github_failing() -> Daemon {
    let mut daemon = daemon();
    for repository in &mut daemon.status.repositories {
        repository.listing = Some(match repository.name.as_str() {
            LEDGER => api::ListingFailure {
                since: NOW - 600,
                problem: api::ListingProblem::Refused { status: 404 },
            },
            _ => api::ListingFailure {
                since: NOW - 90,
                problem: api::ListingProblem::Unreachable,
            },
        });
    }
    daemon
}

/// The fixtures with GitHub refusing runnerd's token (401) for every repository: one item, not one each.
pub fn github_refused() -> Daemon {
    let mut daemon = daemon();
    for repository in &mut daemon.status.repositories {
        repository.listing = Some(api::ListingFailure {
            since: NOW - 300,
            problem: api::ListingProblem::Unauthorized,
        });
    }
    daemon
}

/// The fixtures with halyard/storefront paused: its queued jobs wait for a resume.
pub fn paused() -> Daemon {
    let mut daemon = daemon();
    for repository in &mut daemon.status.repositories {
        repository.paused = repository.name == STOREFRONT;
    }
    for queued in &mut daemon.status.queued {
        if queued.repository == STOREFRONT && queued.waiting != Some(Waiting::RunnerStarting) {
            queued.waiting = Some(Waiting::Paused);
        }
    }
    daemon
}

/// `GET /v1/attention` at revision 1 of the instance `fixtures` for `daemon`, `doctor` and the insights at [`NOW`]:
/// core's items (`api::attention_items`). For the fixtures: the doctor's failure, the token or the listings, labels no
/// pool serves, the long wait for the host's memory, the warning insights and the rate limit.
pub fn attention(daemon: &Daemon, doctor: &api::Doctor) -> api::Attention {
    api::Attention::new(
        "fixtures".to_owned(),
        1,
        api::attention_items(&daemon.status, doctor, Some(&insights(NOW)), NOW),
    )
}

/// More than an attention list holds: 60 doctor warnings, of which [`api::ATTENTION_MAX`] are listed.
pub fn crowded_attention() -> api::Attention {
    let mut doctor = doctor();
    doctor.checks = (1..=60)
        .map(|n| api::Check {
            name: format!("volume {n:02}"),
            status: api::CheckStatus::Warn,
            detail: format!("runnerd-node-slot-{n} is over its size"),
            fix: None,
        })
        .collect();
    attention(&daemon(), &doctor)
}

/// `GET /v1/attention` from a newer daemon: a kind, a tone, a subject and a fix this version does not know, beside an
/// item it does. A client shows the first by its title, neutral, without a link or a fix.
pub const FUTURE_ATTENTION: &str = r#"{
  "instance": "7f3a9c-1",
  "revision": 12,
  "items": [
    {"key": "disk-full", "kind": "disk-full", "tone": "alarm", "title": "Docker's disk is 98% full",
     "subject": {"kind": "volume", "name": "runnerd-pnpm"}, "since": 1792677000,
     "fix": {"kind": "wizard", "steps": 3}},
    {"key": "doctor:sysbox", "kind": "doctor", "tone": "bad", "title": "sysbox: sysbox-runc is not a Docker runtime",
     "subject": {"kind": "check", "name": "sysbox"}, "fix": {"kind": "open", "view": "timeline"}}
  ],
  "omitted": 2
}"#;

/// One attention item as every client states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionFact {
    /// What a client keys it by.
    pub key: String,
    /// Its line.
    pub title: String,
    /// What it means.
    pub tone: Tone,
    /// What clears it, in core's words (`api::Fix::line`).
    pub fix: Option<String>,
    /// How old its evidence is (`api::ago`); `None` when unknown.
    pub since: Option<String>,
}

/// What every client states of an attention list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionFacts {
    /// Its items, in its order.
    pub items: Vec<AttentionFact>,
    /// What a client that shows some of them says of the rest: `and 3 more`; `None` when it shows all.
    pub more: Option<String>,
}

/// The facts of `attention`, as core words them at [`NOW`], for a client that shows `shown` items.
pub fn attention_facts(attention: &api::Attention, shown: usize) -> AttentionFacts {
    let items = attention
        .items
        .iter()
        .map(|item| AttentionFact {
            key: item.key.clone(),
            title: item.title.clone(),
            tone: item.tone,
            fix: item.fix.as_ref().map(api::Fix::line),
            since: item.since.map(|at| api::ago(at, NOW)),
        })
        .collect();
    AttentionFacts {
        items,
        more: attention.more(shown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures are as tricky as their docs say; a change that makes them easy fails here, not silently in
    /// every client's test.
    #[test]
    fn the_fixtures_hold_the_cases_clients_drifted_on() {
        let daemon = daemon();
        let facts = facts(&daemon);
        let label = |job: u64| {
            facts
                .rows
                .iter()
                .find(|row| row.job == job)
                .map(|row| row.label.as_str())
        };
        let insights = insights(NOW);
        let held: Vec<api::Severity> = insights
            .insights
            .iter()
            .filter(|insight| matches!(insight.kind, api::InsightKind::CpuHeld { .. }))
            .map(|insight| insight.severity)
            .collect();
        assert_eq!(
            held,
            [api::Severity::Warning, api::Severity::Notice],
            "a CPU cap held back as a warning and as a notice"
        );
        assert_eq!(label(LOST), Some("lost"));
        assert_eq!(label(ELSEWHERE), Some("elsewhere"));
        assert_eq!(label(NEVER_STARTED), Some("cancelled in queue"));
        assert_eq!(label(STARTUP_FAILURE), Some("startup failure"));
        assert_eq!(label(TIMED_OUT), Some("timed out"));
        assert_eq!(label(CANCELLED), Some("cancelled"));
        assert_eq!(label(RUNNER_FAULT), Some("runner fault"));
        assert!(
            facts.rows.iter().any(|row| row.title == "job 9206"),
            "the unnamed job is named by its ID"
        );
        assert_eq!((facts.job.failed, facts.job.finished), (2, 4));
        assert_eq!(facts.job.steps[2].took, None);
        assert!(
            facts.job.steps[3].took.is_some(),
            "a cancelled step took time"
        );
        assert_eq!(facts.job.limits, "capped at 40G and 1.3 CPUs");
        assert!(
            !daemon.status.budget.held,
            "the measured budget is not held"
        );
        assert_eq!(
            daemon.status.budget.host_withheld(),
            Some(7 * GIB / 4),
            "the host holds back 1.75G of the 2.25G MemoryMax leaves the pooled count"
        );
        assert_eq!(
            facts.budget,
            "21.75G of 24G counted in runnerd.slice by predicted peaks: 22G reserved, 256M pooled; what runs uses 15G \
             and keeps 6.75G for its growth"
        );
        let words: Vec<&str> = facts.slots.iter().map(|slot| slot.word).collect();
        assert_eq!(words, ["running", "warm", "starting", "starting"]);
        assert_eq!(facts.records_tone, Tone::Warn);
        assert!(
            facts.waits[0].reason.starts_with("No profile serves"),
            "{:?}",
            facts.waits
        );
        assert_eq!(
            facts.savings,
            "This month: 29 job-minutes here, about $0.62 at GitHub's rates for each profile's runner before your plan's \
             free minutes (5 jobs not priced)"
        );
        assert!(
            facts
                .savings_30_days
                .starts_with("Last 30 days: 39 job-minutes here, about $0.68"),
            "{}",
            facts.savings_30_days
        );
    }

    /// What needs attention in the fixtures holds each source, in core's order, and the awkward cases say so.
    #[test]
    fn the_attention_fixtures_hold_every_source_and_the_awkward_cases() {
        let kinds = |attention: &api::Attention| -> Vec<api::AttentionKind> {
            attention.items.iter().map(|item| item.kind).collect()
        };
        let attention = attention(&github_failing(), &doctor());
        let found = kinds(&attention);
        for kind in [
            api::AttentionKind::Doctor,
            api::AttentionKind::GithubListing,
            api::AttentionKind::CannotStart,
            api::AttentionKind::HostMemory,
            api::AttentionKind::Insight,
            api::AttentionKind::RateLimited,
        ] {
            assert!(found.contains(&kind), "{kind:?} in {found:?}");
        }
        assert_eq!(attention.items[0].key, "doctor:sysbox", "the failure first");
        assert!(
            !attention
                .items
                .iter()
                .any(|item| item.key.starts_with("insight:host-held")),
            "the host-held insight is the host-memory item"
        );
        assert_eq!(
            kinds(&super::attention(&github_refused(), &doctor()))
                .iter()
                .filter(|kind| **kind == api::AttentionKind::GithubAuth)
                .count(),
            1,
            "one item for a refused token"
        );
        assert!(
            super::attention(&paused(), &doctor())
                .items
                .iter()
                .any(|item| item.key == "cannot-start:paused:halyard/storefront"),
            "a paused repository's jobs"
        );
        for doctor in doctor_not_current() {
            let unavailable = kinds(&super::attention(&daemon(), &doctor));
            assert!(
                unavailable.contains(&api::AttentionKind::DoctorUnavailable)
                    && !unavailable.contains(&api::AttentionKind::Doctor),
                "{:?}: says so, with none of the old checks",
                doctor.state
            );
        }
        let crowded = crowded_attention();
        assert_eq!(crowded.items.len(), api::ATTENTION_MAX);
        assert!(crowded.omitted > 0, "some are left out");
        assert!(attention_facts(&crowded, 3).more.is_some(), "and N more");
        let future: api::Attention = serde_json::from_str(FUTURE_ATTENTION).unwrap();
        assert_eq!(future.items[0].kind, api::AttentionKind::Unknown);
        assert_eq!(future.items[1].fix, None, "a view this version lacks");
    }
}
