//! A fake daemon for the dashboard's tests and the `demo` example: the routes of `runnerd_core::api` on a Unix
//! socket, answered from made-up fixtures (two profiles, `halyard/ledger` on jvm and `halyard/storefront` on
//! node), with an event stream the caller feeds.

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::similar_names,
    dead_code,
    unreachable_pub,
    reason = "test and demo support: a failure here fails the test, and each user uses part of it"
)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{Stream, stream};
use runnerd_core::api::{
    Activity, ActivityQuery, Bucket, Budget, BuildCacheCounts, BuildCacheStats, Busy, CacheHistory,
    CacheKind, CacheSize, CacheVolume, Caches, Check, CheckStatus, ConfigCheck, ConfigFile, Doctor,
    ErrorResponse, Event, EventEnvelope, EventQuery, HostSample, HostSamples, Job, JobMetrics,
    JobPage, JobQuery, JobRunner, JobState, JobSummary, LogQuery, LogSummary, PauseQuery, Paused,
    ProfileStatus, QueuedJob, Record, RecordLimits, RecordNote, RecordRemoval, Records,
    RemoveRecordsQuery, RemovedRecords, ReplaceQuery, RepositoryStatus, RunnerStatus, RunningJob,
    Sample, SampleSeries, SlotStatus, Started, Stats, StatsRow, Status, Step, Timeline,
};
use runnerd_core::api::{
    Advice, JobSizing, Peaks, ProfileSizing, Resize, Sizing, SizingChange, Spread, Starts,
};
use runnerd_core::api::{
    AutoNote, Digest, Insight, InsightKind, Insights, LongWait, RepositoryWaits, RunLink, Severity,
    WaitSpread, WeekWaits,
};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// What the fake daemon answers with.
#[derive(Debug, Clone)]
pub struct Fixtures {
    pub now: u64,
    pub status: Status,
    /// Newest first.
    pub jobs: Vec<Job>,
    pub samples: BTreeMap<u64, Vec<Sample>>,
    pub logs: BTreeMap<u64, Vec<String>>,
    pub host: Vec<HostSample>,
    pub stats: Vec<StatsRow>,
    pub caches: Caches,
    pub cache_history: Vec<CacheSize>,
    pub records: Records,
    /// What `GET /v1/config` reads and `PUT /v1/config` replaces.
    pub config: String,
    /// Written over `config` right after the next `PUT /v1/config` answers, as auto sizing may.
    pub after_put: Option<String>,
    /// The requests to the control endpoints, such as `POST /v1/profiles/jvm/reseed`.
    pub controls: Vec<String>,
    /// Whether the job history fails to record a pause or resume of every repository, as the daemon's 500.
    pub unrecorded: bool,
    /// What `GET /v1/sizing` says auto sizing does next; `None`, auto sizing is off.
    pub auto_plan: Option<runnerd_core::api::AutoPlan>,
    /// What `GET /v1/savings` answers; `None` prices the jobs at GitHub's 2-core rate, every repository private.
    pub savings: Option<runnerd_core::api::Savings>,
}

/// A small deterministic noise source, so fixtures look measured and stay the same.
struct Noise(u64);

impl Noise {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below.max(1)
    }
}

fn step(number: u32, name: &str, from: u64, seconds: Option<u64>, conclusion: &str) -> Step {
    Step {
        number,
        name: name.to_owned(),
        status: if seconds.is_some() {
            "completed"
        } else {
            "in_progress"
        }
        .to_owned(),
        conclusion: seconds.map(|_| conclusion.to_owned()),
        started_at: Some(from),
        completed_at: seconds.map(|seconds| from + seconds),
    }
}

/// Steps of a Gradle build, from `start`; the step running at `now` is in progress.
fn steps(start: u64, now: Option<u64>, names: &[(&str, u64)], failed: Option<usize>) -> Vec<Step> {
    let mut at = start;
    let mut steps = Vec::new();
    for (i, (name, seconds)) in names.iter().enumerate() {
        let number = u32::try_from(i).unwrap() + 1;
        if now.is_some_and(|now| at > now) {
            steps.push(Step {
                number,
                name: (*name).to_owned(),
                status: "pending".to_owned(),
                conclusion: None,
                started_at: None,
                completed_at: None,
            });
            continue;
        }
        if now.is_some_and(|now| at + seconds > now) {
            steps.push(step(number, name, at, None, ""));
            // Later steps start after `now`: pending.
            at = now.unwrap_or(at) + 1;
            continue;
        }
        let conclusion = match failed {
            Some(failed) if failed == i => "failure",
            Some(failed) if i > failed && i + 1 < names.len() => "skipped",
            _ => "success",
        };
        steps.push(step(number, name, at, Some(*seconds), conclusion));
        at += seconds;
    }
    steps
}

const LEDGER_STEPS: [(&str, u64); 9] = [
    ("Set up job", 3),
    ("Run actions/checkout@v6", 4),
    ("Set up JDK 21", 6),
    ("Restore Gradle cache", 9),
    ("Build", 148),
    ("Unit tests", 212),
    ("Integration tests (Testcontainers)", 281),
    ("Post Run actions/checkout@v6", 1),
    ("Complete job", 1),
];

const STOREFRONT_STEPS: [(&str, u64); 8] = [
    ("Set up job", 2),
    ("Run actions/checkout@v6", 3),
    ("Set up pnpm", 5),
    ("pnpm install --frozen-lockfile", 41),
    ("Build", 96),
    ("Playwright e2e", 388),
    ("Post Run actions/checkout@v6", 1),
    ("Complete job", 1),
];

struct Spec<'a> {
    id: u64,
    repository: &'a str,
    workflow: &'a str,
    name: &'a str,
    branch: &'a str,
    profile: &'a str,
    slot: usize,
    queued_at: u64,
    wait: u64,
    steps: &'a [(&'a str, u64)],
    failed: Option<usize>,
    memory: u64,
}

fn job(spec: &Spec<'_>, now: u64, noise: &mut Noise) -> (Job, Vec<Sample>, Vec<String>) {
    let container = spec.queued_at + spec.wait / 2;
    let started = spec.queued_at + spec.wait;
    let length: u64 = spec.steps.iter().map(|(_, seconds)| seconds).sum();
    let running = started + length > now;
    let finished = (!running).then_some(started + length);
    let steps = steps(started, running.then_some(now), spec.steps, spec.failed);
    let end = finished.unwrap_or(now);
    let mut samples = Vec::new();
    let mut received = 0;
    let mut at = container;
    while at <= end {
        let progress = (at - container) * 100 / (end - container).max(1);
        let memory = spec.memory * (35 + progress.min(60)) / 100 + noise.next(spec.memory / 10);
        let cpu = 400 + noise.next(if progress > 20 { 7200 } else { 2000 });
        received += noise.next(3 * MIB);
        samples.push(Sample {
            at,
            cpu,
            memory,
            network_received: received,
            network_sent: received / 9,
        });
        at += 2;
    }
    let memory_peak = samples.iter().map(|s| s.memory).max();
    let cpu_peak = samples.iter().map(|s| s.cpu).max();
    let count = samples.len().max(1) as u64;
    let cpu_time = samples.iter().map(|s| s.cpu * 2).sum::<u64>();
    let lines: Vec<String> = steps
        .iter()
        .flat_map(|step| {
            let name = step.name.clone();
            (0..6).map(move |i| match i {
                0 => format!("##[group]{name}"),
                5 => "##[endgroup]".to_owned(),
                _ => format!("{name}: line {i} of what the step printed"),
            })
        })
        .collect();
    let conclusion = finished.map(|_| {
        if spec.failed.is_some() {
            "failure".to_owned()
        } else {
            "success".to_owned()
        }
    });
    let summary = JobSummary {
        id: spec.id,
        repository: spec.repository.to_owned(),
        run_id: spec.id / 3,
        run_attempt: 1,
        workflow: Some(spec.workflow.to_owned()),
        name: spec.name.to_owned(),
        branch: Some(spec.branch.to_owned()),
        url: Some(format!(
            "https://github.com/{}/actions/runs/{}/job/{}",
            spec.repository,
            spec.id / 3,
            spec.id
        )),
        labels: vec!["self-hosted".to_owned(), spec.profile.to_owned()],
        state: if running {
            JobState::Running
        } else {
            JobState::Completed
        },
        conclusion,
        runner: Some(JobRunner {
            name: format!(
                "runnerd-3f9a-forge-{}-{}-{}",
                spec.profile, spec.slot, container
            ),
            profile: spec.profile.to_owned(),
            slot: spec.slot,
            memory_cap: Some(if spec.profile == "jvm" {
                9 * GIB
            } else {
                7 * GIB
            }),
            cpu_cap: Some(8000),
        }),
        timeline: Timeline {
            queued_at: spec.queued_at,
            container_started_at: Some(container),
            started_at: Some(started),
            finished_at: finished,
            cleaned_at: finished.map(|finished| finished + 4),
            unseen: 0,
        },
        metrics: JobMetrics {
            cpu_time: Some(cpu_time),
            cpu_peak,
            cpu_average: Some(cpu_time / 2 / count),
            memory_peak,
            memory_average: Some(samples.iter().map(|s| s.memory).sum::<u64>() / count),
            network_received: Some(received),
            network_sent: Some(received / 9),
            mirror_bytes: received / 3,
            build_cache: BuildCacheCounts {
                hits: count % 40 + 10,
                misses: count % 7 + 2,
                read_bytes: received / 5,
                written_bytes: received / 40,
                refused: 0,
                writer: true,
            },
            sock_throttled: Some(0),
            // Busy for most of its run, its bursts held at the cap in some of it.
            cpu_periods: Some(count * 18),
            cpu_throttled: Some(count * 18 * (spec.slot % 3) as u64 / 20),
            cpu_stalled: Some(count * 2000 * ((spec.slot % 3) as u64 + 1) / 8),
        },
        log: LogSummary {
            lines: lines.len() as u64,
            bytes: lines.iter().map(|line| line.len() as u64 + 1).sum(),
            truncated: false,
        },
    };
    (
        Job {
            admission: None,
            summary,
            steps,
            record: None,
        },
        samples,
        lines,
    )
}

fn slot(
    repository: Option<&str>,
    profile: &str,
    slot: usize,
    started_at: u64,
    busy: bool,
) -> SlotStatus {
    SlotStatus {
        slot,
        runner: repository.map(|repository| RunnerStatus {
            repository: repository.to_owned(),
            name: format!("runnerd-3f9a-forge-{profile}-{slot}-{started_at}"),
            runner_id: 4100 + started_at % 1000,
            container: format!("{started_at:x}c0ffee"),
            busy,
            started_at,
            reserved: if profile == "jvm" { 9 * GIB } else { 7 * GIB },
            warm: false,
        }),
    }
}

impl Fixtures {
    /// The made-up machine at `now`: one job running in each profile's slot 1, a runner waiting in node's slot 2,
    /// and 30 days of history.
    pub fn new(now: u64) -> Fixtures {
        let mut noise = Noise(7);
        let ledger = Spec {
            id: 120_055_991_700,
            repository: "halyard/ledger",
            workflow: "CI",
            name: "build",
            branch: "main",
            profile: "jvm",
            slot: 1,
            queued_at: now - 431,
            wait: 9,
            steps: &LEDGER_STEPS,
            failed: None,
            memory: 9 * GIB * 7 / 10,
        };
        let storefront = Spec {
            id: 120_055_991_650,
            repository: "halyard/storefront",
            workflow: "E2E",
            name: "playwright",
            branch: "feat/checkout-v2",
            profile: "node",
            slot: 1,
            queued_at: now - 312,
            wait: 7,
            steps: &STOREFRONT_STEPS,
            failed: None,
            memory: 7 * GIB * 6 / 10,
        };
        let mut jobs = Vec::new();
        let mut samples = BTreeMap::new();
        let mut logs = BTreeMap::new();
        let mut add = |spec: &Spec<'_>, noise: &mut Noise| {
            let (job, job_samples, lines) = job(spec, now, noise);
            samples.insert(job.summary.id, job_samples);
            logs.insert(job.summary.id, lines);
            jobs.push(job);
        };
        add(&ledger, &mut noise);
        add(&storefront, &mut noise);
        // Finished jobs, one every 40 minutes or so, newest first.
        for i in 0..24u64 {
            let ledger_turn = i % 3 != 1;
            let spec = Spec {
                id: 120_055_991_600 - i * 37,
                repository: if ledger_turn {
                    "halyard/ledger"
                } else {
                    "halyard/storefront"
                },
                workflow: if ledger_turn { "CI" } else { "E2E" },
                name: if ledger_turn { "build" } else { "playwright" },
                branch: ["main", "fix/rounding", "feat/checkout-v2", "main"]
                    [usize::try_from(i % 4).unwrap()],
                profile: if ledger_turn { "jvm" } else { "node" },
                slot: usize::try_from(i % 2).unwrap() + 1,
                queued_at: now - 1500 - i * 2400 - noise.next(600),
                wait: 4 + noise.next(30),
                steps: if ledger_turn {
                    &LEDGER_STEPS
                } else {
                    &STOREFRONT_STEPS
                },
                failed: (i % 7 == 3).then_some(5),
                memory: if ledger_turn {
                    9 * GIB * 7 / 10
                } else {
                    7 * GIB / 2
                },
            };
            add(&spec, &mut noise);
        }
        let host = (0..900u64)
            .rev()
            .map(|i| {
                let at = now - i * 2;
                let busy = u64::from(at > now - 440) + u64::from(at > now - 320);
                HostSample {
                    at,
                    cpu: 300 + busy * 3000 + noise.next(2500),
                    memory: 600 * MIB + busy * 4 * GIB + noise.next(GIB),
                }
            })
            .collect();
        let today = now - now % 86_400;
        let mut stats = Vec::new();
        for day in 0..30u64 {
            for (repository, base) in [("halyard/ledger", 9u64), ("halyard/storefront", 5)] {
                let jobs = base + noise.next(8) + if day % 7 < 2 { 0 } else { 6 };
                let failed = noise.next(3);
                let cancelled = noise.next(2);
                stats.push(StatsRow {
                    start: today - (29 - day) * 86_400,
                    repository: repository.to_owned(),
                    jobs,
                    succeeded: jobs - failed - cancelled,
                    failed,
                    cancelled,
                    queue_seconds: jobs * (6 + noise.next(25)),
                    run_seconds: jobs * (420 + noise.next(260)),
                    cpu_time: jobs * 900_000,
                    memory_peak: if base == 9 { 6 * GIB } else { 4 * GIB } + noise.next(GIB),
                    network_received: jobs * 300 * MIB,
                    mirror_bytes: jobs * 120 * MIB,
                });
            }
        }
        let volumes = vec![
            (
                "runnerd-jvm-base",
                CacheKind::Snapshot {
                    profile: "jvm".to_owned(),
                },
                14 * GIB,
                None,
            ),
            (
                "runnerd-jvm-slot-1",
                CacheKind::Slot {
                    profile: "jvm".to_owned(),
                    slot: 1,
                },
                1800 * MIB,
                None,
            ),
            (
                "runnerd-jvm-slot-2",
                CacheKind::Slot {
                    profile: "jvm".to_owned(),
                    slot: 2,
                },
                640 * MIB,
                None,
            ),
            (
                "runnerd-node-base",
                CacheKind::Snapshot {
                    profile: "node".to_owned(),
                },
                9 * GIB,
                None,
            ),
            (
                "runnerd-node-slot-1",
                CacheKind::Slot {
                    profile: "node".to_owned(),
                    slot: 1,
                },
                950 * MIB,
                None,
            ),
            (
                "runnerd-node-slot-2",
                CacheKind::Slot {
                    profile: "node".to_owned(),
                    slot: 2,
                },
                300 * MIB,
                None,
            ),
            (
                "runnerd-pnpm",
                CacheKind::Pnpm,
                3 * GIB + 400 * MIB,
                Some(5 * GIB),
            ),
            (
                "runnerd-maven",
                CacheKind::Maven,
                4 * GIB + 100 * MIB,
                Some(5 * GIB),
            ),
            ("runnerd-mirror", CacheKind::Mirror, 7 * GIB, None),
            (
                "runnerd-build-cache",
                CacheKind::Build,
                6 * GIB + 300 * MIB,
                Some(10 * GIB),
            ),
        ];
        let mut cache_history = Vec::new();
        for (name, _, bytes, limit) in &volumes {
            let mut size = bytes / 3;
            for day in 0..30u64 {
                size = (size + bytes / 25 + noise.next(bytes / 20)).min(*bytes);
                if limit.is_some_and(|limit| size > limit * 9 / 10) && day % 11 == 10 {
                    size = bytes / 4;
                }
                cache_history.push(CacheSize {
                    at: today - (29 - day) * 86_400 + 3600,
                    name: (*name).to_owned(),
                    bytes: if day == 29 { *bytes } else { size },
                });
            }
        }
        cache_history.sort_by_key(|size| size.at);
        let caches = Caches {
            measured_at: Some(now - 900),
            volumes: volumes
                .into_iter()
                .map(|(name, kind, bytes, limit)| CacheVolume {
                    name: name.to_owned(),
                    kind,
                    bytes,
                    limit,
                })
                .collect(),
            build: Some(BuildCacheStats {
                bytes: 6 * GIB + 300 * MIB,
                entries: 18_412,
                limit: 10 * GIB,
                week: BuildCacheCounts {
                    hits: 41_870,
                    misses: 9_215,
                    read_bytes: 38 * GIB,
                    written_bytes: 5 * GIB,
                    refused: 1_204,
                    writer: false,
                },
                actions_cache: true,
            }),
        };
        let status = Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            shutting_down: false,
            budget: Budget {
                admission: runnerd_core::api::AdmissionPolicy::Caps,
                held: false,
                slice: "runnerd.slice".to_owned(),
                memory_max: Some(33 * GIB),
                memory_in_use: 9 * GIB + 7 * GIB + 7 * GIB + 512 * MIB,
                mirror_memory: 512 * MIB,
                seed_memory: Vec::new(),
                shell_memory: Vec::new(),
                slice_cpus: Some(32_000),
                host: None,
                cpu: None,
            },
            profiles: vec![
                ProfileStatus {
                    name: "jvm".to_owned(),
                    image: "runnerd-runner:jvm".to_owned(),
                    cpus: 8,
                    memory: 9 * GIB,
                    reseeding: false,
                    slots: vec![
                        slot(Some("halyard/ledger"), "jvm", 1, ledger.queued_at + 4, true),
                        slot(None, "jvm", 2, 0, false),
                    ],
                },
                ProfileStatus {
                    name: "node".to_owned(),
                    image: "runnerd-runner:node".to_owned(),
                    cpus: 8,
                    memory: 7 * GIB,
                    reseeding: false,
                    slots: vec![
                        slot(
                            Some("halyard/storefront"),
                            "node",
                            1,
                            storefront.queued_at + 3,
                            true,
                        ),
                        slot(Some("halyard/storefront"), "node", 2, now - 12, false),
                    ],
                },
            ],
            repositories: vec![
                RepositoryStatus {
                    name: "halyard/ledger".to_owned(),
                    pools: vec![runnerd_core::api::PoolStatus {
                        profile: "jvm".to_owned(),
                        labels: vec![
                        "self-hosted".to_owned(),
                        "linux".to_owned(),
                        "x64".to_owned(),
                        "jvm".to_owned(),
                    ],
                        queued_jobs: 0,
                    }],
                    paused: false,
                    queued_jobs: 0,
                },
                RepositoryStatus {
                    name: "halyard/storefront".to_owned(),
                    pools: vec![runnerd_core::api::PoolStatus {
                        profile: "node".to_owned(),
                        labels: vec![
                        "self-hosted".to_owned(),
                        "linux".to_owned(),
                        "x64".to_owned(),
                        "node".to_owned(),
                    ],
                        queued_jobs: 1,
                    }],
                    paused: false,
                    queued_jobs: 1,
                },
            ],
            queued: vec![QueuedJob {
                id: 120_055_991_710,
                repository: "halyard/storefront".to_owned(),
                profile: None,
                run_id: 120_055_991_710 / 3,
                run_attempt: 1,
                workflow: Some("E2E".to_owned()),
                name: "visual-diff".to_owned(),
                branch: Some("feat/checkout-v2".to_owned()),
                url: Some(
                    "https://github.com/halyard/storefront/actions/runs/40018663903/job/120055991710"
                        .to_owned(),
                ),
                queued_at: now - 41,
                waiting: Some(runnerd_core::api::Waiting::RunnerStarting),
            }],
        };
        jobs.sort_by_key(|job| std::cmp::Reverse(job.summary.id));
        // Each failed job left a record, the newest last as the daemon lists them.
        let mut records: Vec<Record> = jobs
            .iter_mut()
            .filter(|job| job.summary.conclusion.as_deref() == Some("failure"))
            .map(|job| {
                let summary = &job.summary;
                let created_at = summary.timeline.finished_at.unwrap_or(now);
                let bytes = 280 * MIB + noise.next(400) * MIB;
                job.record = Some(RecordNote {
                    created_at: Some(created_at),
                    bytes: Some(bytes),
                    ..RecordNote::default()
                });
                Record {
                    job: summary.id,
                    repository: summary.repository.clone(),
                    profile: summary
                        .runner
                        .as_ref()
                        .map(|runner| runner.profile.clone())
                        .unwrap_or_default(),
                    workflow: summary.workflow.clone(),
                    name: Some(summary.name.clone()),
                    created_at,
                    bytes,
                    image: format!("runnerd-record:{}", summary.id),
                }
            })
            .collect();
        records.reverse();
        let records = Records {
            records,
            limits: RecordLimits {
                max_total: 20 * GIB,
                max_count: 10,
                max_age: 7 * 86_400,
                min_free: 10 * GIB,
            },
            shells: Vec::new(),
        };
        Fixtures {
            now,
            status,
            jobs,
            samples,
            logs,
            host,
            stats,
            caches,
            cache_history,
            records,
            config: include_str!("../../../../examples/runnerd.toml").to_owned(),
            after_put: None,
            controls: Vec::new(),
            unrecorded: false,
            auto_plan: None,
            savings: None,
        }
    }

    /// The jobs `query` lists, as the daemon pages them.
    /// As the daemon reads it: the jobs started and finished since `since`, by those times, for the day from
    /// `since`. The fixtures' clock is not the dashboard's, so nothing is left out for being after `now`.
    fn activity(&self, since: Option<u64>) -> Activity {
        let since = since.unwrap_or(self.now.saturating_sub(24 * 3600));
        let within = |at: Option<u64>| at.is_some_and(|at| at >= since);
        let summaries = || self.jobs.iter().map(|job| &job.summary);
        let mut started: Vec<Started> = summaries()
            .filter(|job| job.state != JobState::Elsewhere && within(job.timeline.started_at))
            .filter_map(|job| {
                let at = job.timeline.started_at?;
                Some(Started {
                    at,
                    waited: at.saturating_sub(job.timeline.queued_at),
                })
            })
            .collect();
        started.sort_by_key(|job| job.at);
        let finished: Vec<&JobSummary> = summaries()
            .filter(|job| {
                matches!(job.state, JobState::Completed | JobState::Lost)
                    && within(job.timeline.finished_at)
            })
            .collect();
        Activity {
            since,
            until: since + 24 * 3600,
            started,
            finished: finished.len() as u64,
            failed: finished
                .iter()
                .filter(|job| {
                    job.state == JobState::Lost
                        || matches!(job.conclusion.as_deref(), Some("failure" | "timed_out"))
                })
                .count() as u64,
            // The fixtures keep no kills for memory: only lost jobs are their runner's failures.
            runner_failed: finished
                .iter()
                .filter(|job| job.state == JobState::Lost)
                .count() as u64,
        }
    }

    fn page(&self, query: &JobQuery) -> JobPage {
        let limit = query.limit.unwrap_or(50) as usize;
        let mut jobs: Vec<JobSummary> = self
            .jobs
            .iter()
            .map(|job| job.summary.clone())
            .filter(|job| {
                query
                    .repository
                    .as_ref()
                    .is_none_or(|r| *r == job.repository)
            })
            .filter(|job| query.state.is_none_or(|state| state == job.state))
            .filter(|job| {
                query
                    .conclusion
                    .as_ref()
                    .is_none_or(|c| job.conclusion.as_ref() == Some(c))
            })
            .filter(|job| {
                query
                    .workflow
                    .as_ref()
                    .is_none_or(|workflow| job.workflow.as_ref() == Some(workflow))
            })
            .filter(|job| query.name.as_ref().is_none_or(|name| job.name == *name))
            .filter(|job| query.before.is_none_or(|before| job.id < before))
            .filter(|job| {
                query
                    .since
                    .is_none_or(|since| job.timeline.queued_at >= since)
            })
            .filter(|job| {
                query
                    .until
                    .is_none_or(|until| job.timeline.queued_at < until)
            })
            .collect();
        let next = (jobs.len() > limit).then(|| jobs[limit - 1].id);
        jobs.truncate(limit);
        JobPage { jobs, next }
    }
}

/// The fake daemon's state: the fixtures, the events it streams, and the writes it was sent.
#[derive(Clone)]
pub struct Fake {
    pub fixtures: Arc<Mutex<Fixtures>>,
    pub events: broadcast::Sender<EventEnvelope>,
}

impl Fake {
    pub fn new(fixtures: Fixtures) -> Fake {
        Fake {
            fixtures: Arc::new(Mutex::new(fixtures)),
            events: broadcast::channel(256).0,
        }
    }
}

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            message: message.to_owned(),
        }),
    )
        .into_response()
}

fn find_job(fake: &Fake, id: u64) -> Option<Job> {
    let fixtures = fake.fixtures.lock().unwrap();
    fixtures
        .jobs
        .iter()
        .find(|job| job.summary.id == id)
        .cloned()
}

/// What the history says: ledger's tests got slower on main, storefront's end-to-end job is flaky, ledger's
/// integration tests near jvm's 9G, and storefront's jobs wait longer than the week before.
fn insights(fixtures: &Fixtures) -> Insights {
    let now = fixtures.now;
    let link = |repository: &str, back: u64, attempt| {
        let job = fixtures
            .jobs
            .iter()
            .find(|job| job.summary.repository == repository)
            .map_or(1, |job| job.summary.id);
        RunLink {
            job,
            run_id: 9_400_000 + job % 1000,
            run_attempt: attempt,
            branch: Some("main".to_owned()),
            url: Some(format!(
                "https://github.com/{repository}/actions/runs/{job}"
            )),
            at: now - back,
        }
    };
    let insight =
        |key: &str, severity, repository: &str, job: Option<&str>, sentence: &str, kind, runs| {
            Insight {
                key: key.to_owned(),
                severity,
                repository: repository.to_owned(),
                workflow: job.map(|_| "CI".to_owned()),
                job: job.map(str::to_owned),
                sentence: sentence.to_owned(),
                kind,
                runs,
            }
        };
    let spread = |jobs, p50, p95| WaitSpread {
        jobs,
        p50: Some(p50),
        p95: Some(p95),
    };
    Insights {
        since: now - 14 * 86_400,
        until: now,
        insights: vec![
            insight(
                "slower:halyard/ledger/CI/test",
                Severity::Warning,
                "halyard/ledger",
                Some("test"),
                "halyard/ledger: CI / test on main takes 6m 10s now, up from 4m 40s.",
                InsightKind::Slower {
                    branch: Some("main".to_owned()),
                    before: 280,
                    now: 370,
                    since: Some(link("halyard/ledger", 3 * 86_400, 1)),
                },
                vec![link("halyard/ledger", 2 * 3600, 1)],
            ),
            insight(
                "near-cap:halyard/ledger/CI/integration",
                Severity::Warning,
                "halyard/ledger",
                Some("integration"),
                "halyard/ledger: CI / integration peaks at 8.1G, 90% of jvm's 9G. Auto sizing is off: raise jvm's \
                 memory, or turn it on.",
                InsightKind::NearCap {
                    profile: "jvm".to_owned(),
                    peak: 81 * GIB / 10,
                    cap: 9 * GIB,
                    auto: AutoNote::Off,
                },
                vec![link("halyard/ledger", 5 * 3600, 1)],
            ),
            insight(
                "flaky:halyard/storefront/CI/e2e",
                Severity::Warning,
                "halyard/storefront",
                Some("e2e"),
                "halyard/storefront: CI / e2e failed and then passed on a re-run in 2 of its 31 runs.",
                InsightKind::Flaky {
                    flaky: 2,
                    total: 31,
                },
                vec![link("halyard/storefront", 26 * 3600, 1)],
            ),
            insight(
                "waits:halyard/storefront",
                Severity::Notice,
                "halyard/storefront",
                None,
                "halyard/storefront: 1 job in 20 waits 4m 10s or more for a runner this week, up from 1m 30s the week \
                 before.",
                InsightKind::Waits {
                    before: 90,
                    now: 250,
                },
                Vec::new(),
            ),
        ],
        waits: vec![
            RepositoryWaits {
                repository: "halyard/ledger".to_owned(),
                now: WeekWaits {
                    all: spread(184, 3, 41),
                    warm: spread(151, 2, 6),
                    cold: spread(33, 24, 58),
                },
                before: WeekWaits {
                    all: spread(170, 4, 44),
                    ..WeekWaits::default()
                },
            },
            RepositoryWaits {
                repository: "halyard/storefront".to_owned(),
                now: WeekWaits {
                    all: spread(212, 9, 250),
                    warm: spread(120, 2, 11),
                    cold: spread(92, 47, 260),
                },
                before: WeekWaits {
                    all: spread(198, 6, 90),
                    ..WeekWaits::default()
                },
            },
        ],
    }
}

/// What the fixtures' jobs would have cost on GitHub's runners, priced by core.
fn savings(fixtures: &Fixtures) -> runnerd_core::api::Savings {
    if let Some(savings) = &fixtures.savings {
        return savings.clone();
    }
    let ms = |seconds: u64| seconds * 1000;
    let history = runnerd_core::api::PricingHistory {
        runs: fixtures
            .jobs
            .iter()
            .map(|job| runnerd_core::api::PricedRun {
                repository: job.summary.repository.clone(),
                workflow: job.summary.workflow.clone(),
                labels: job.summary.labels.clone(),
                state: job.summary.state,
                profile: job
                    .summary
                    .runner
                    .as_ref()
                    .map(|runner| runner.profile.clone()),
                queued_at: ms(job.summary.timeline.queued_at),
                started_at: job.summary.timeline.started_at.map(ms),
                finished_at: job.summary.timeline.finished_at.map(ms),
            })
            .collect(),
        visibility: fixtures
            .jobs
            .iter()
            .map(|job| {
                (
                    job.summary.repository.clone(),
                    runnerd_core::api::Visibility::Private,
                )
            })
            .collect(),
        kept_since: ms(fixtures.now - 60 * 86_400),
    };
    let pricing = runnerd_core::api::Pricing {
        profiles: BTreeMap::new(),
        default_sku: runnerd_core::api::Sku::ActionsLinux,
    };
    runnerd_core::api::Savings::of(&history, &pricing, ms(fixtures.now))
}

/// The last 24 hours of the fixtures.
fn digest(fixtures: &Fixtures) -> Digest {
    let insights = insights(fixtures);
    Digest {
        since: fixtures.now - 86_400,
        until: fixtures.now,
        finished: 57,
        failed: 3,
        new: insights.insights.into_iter().take(1).collect(),
        waits: vec![LongWait {
            job: 1,
            repository: "halyard/storefront".to_owned(),
            workflow: Some("CI".to_owned()),
            name: "e2e".to_owned(),
            waited: 262,
            url: None,
        }],
        savings: Some(savings(fixtures).month),
    }
}

/// The slice the fixtures' config names has a `MemoryMax` of 33G.
/// What the history says of the config's profiles: jvm's tests peak at 7.8G of 9G, so 9G stays; node's builds
/// peak at 2.1G of 7G and wait for a slot, so 3.25G caps, with 1G of page cache room, fit 4 in its 2 × 7G. The
/// end-to-end job has too few runs.
fn sizing(fixtures: &Fixtures) -> Sizing {
    let config = runnerd_core::config::Config::parse(&fixtures.config).unwrap();
    let spread = |p50, p95, p99, max| Spread { p50, p95, p99, max };
    let gib = |tenths: u64| tenths * GIB / 10;
    let job =
        |repository: &str, name: &str, jobs, memory: Spread, wait: Spread, advice| JobSizing {
            repository: repository.to_owned(),
            workflow: Some("CI".to_owned()),
            name: name.to_owned(),
            peaks: Peaks {
                jobs,
                at_cap: 0,
                memory,
                cpu: spread(3100, 6900, 7600, 7900),
                cpu_runs: jobs,
                on_cap: 0,
            },
            wait,
            advice,
        };
    let profiles = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let (busy, wait, memory, advice, jobs) = if name == "jvm" {
                let memory = spread(gib(52), gib(74), gib(77), gib(78));
                let wait = spread(8, 35, 70, 95);
                (
                    41,
                    wait,
                    memory,
                    Advice::Keep { memory: 9 * GIB },
                    vec![
                        job(
                            "halyard/ledger",
                            "build",
                            212,
                            spread(gib(31), gib(42), gib(45), gib(46)),
                            spread(7, 30, 66, 90),
                            Advice::Lower {
                                memory: 5888 * MIB,
                                slots: 2,
                            },
                        ),
                        job(
                            "halyard/ledger",
                            "integration tests",
                            188,
                            memory,
                            wait,
                            Advice::Keep { memory: 9 * GIB },
                        ),
                    ],
                )
            } else {
                let memory = spread(gib(14), gib(19), gib(20), gib(21));
                let wait = spread(12, 260, 430, 610);
                (
                    78,
                    wait,
                    memory,
                    Advice::Lower {
                        memory: 3328 * MIB,
                        slots: 4,
                    },
                    vec![
                        job(
                            "halyard/storefront",
                            "build",
                            236,
                            memory,
                            wait,
                            Advice::Lower {
                                memory: 3328 * MIB,
                                slots: 4,
                            },
                        ),
                        job(
                            "halyard/storefront",
                            "e2e",
                            6,
                            spread(gib(18), gib(20), gib(20), gib(20)),
                            spread(15, 280, 280, 280),
                            Advice::TooFew { jobs: 6 },
                        ),
                    ],
                )
            };
            let advice = match advice {
                Advice::Lower { memory, .. } if profile.memory.0 <= memory => {
                    Advice::Keep { memory }
                }
                advice => advice,
            };
            ProfileSizing {
                name: name.clone(),
                memory: profile.memory.0,
                cpus: profile.cpus,
                slots: profile.slots,
                peaks: Peaks {
                    jobs: jobs.iter().map(|job| job.peaks.jobs).sum(),
                    at_cap: 0,
                    memory,
                    cpu: spread(3100, 6900, 7600, 7900),
                    cpu_runs: jobs.iter().map(|job| job.peaks.cpu_runs).sum(),
                    on_cap: 0,
                },
                wait,
                starts: Starts {
                    warm: 31,
                    warm_p50: 2,
                    cold: 9,
                    cold_p50: 21,
                },
                busy,
                wants_slots: busy >= 50,
                advice,
                jobs,
                revert: None,
            }
        })
        .collect();
    Sizing {
        auto_plan: fixtures.auto_plan.clone(),
        auto: fixtures.auto_plan.is_some(),
        // Auto sizing lowered jvm two days ago.
        changes: vec![SizingChange {
            at: fixtures.now - 2 * 86_400,
            profile: "jvm".to_owned(),
            from: Resize {
                memory: 10 * GIB,
                slots: 2,
                cpus: None,
            },
            to: Resize {
                memory: 9 * GIB,
                slots: 2,
                cpus: None,
            },
            reason: "lower memory to 9G from 10G, its largest peak + 15%".to_owned(),
            uncertain: false,
        }],
        since: fixtures.now - 30 * 86_400,
        until: fixtures.now,
        margin: 15,
        sample_interval: 2,
        min_jobs: 10,
        memory_max: Some(33 * GIB),
        admission: runnerd_core::api::AdmissionPolicy::Caps,
        profiles,
    }
}

fn check(text: &str) -> (Option<runnerd_core::config::Config>, ConfigCheck) {
    runnerd_core::config::Config::check(text, |_| {
        Ok(Some(runnerd_core::config::ByteSize(33 * GIB)))
    })
}

fn running_jobs(fixtures: &Fixtures, repository: &str) -> Vec<RunningJob> {
    fixtures
        .jobs
        .iter()
        .filter(|job| {
            job.summary.state == JobState::Running && job.summary.repository == repository
        })
        .map(|job| RunningJob {
            repository: repository.to_owned(),
            runner: job
                .summary
                .runner
                .as_ref()
                .map(|runner| runner.name.clone())
                .unwrap_or_default(),
            job: Some(job.summary.id),
        })
        .collect()
}

fn pause(fake: &Fake, repository: &str, force: bool) -> Response {
    let mut fixtures = fake.fixtures.lock().unwrap();
    fixtures
        .controls
        .push(format!("pause {repository} force={force}"));
    let running = running_jobs(&fixtures, repository);
    if !running.is_empty() && !force {
        let message = format!("{repository} runs {}", running[0]);
        return (
            StatusCode::CONFLICT,
            Json(Busy {
                message,
                jobs: running,
            }),
        )
            .into_response();
    }
    drop(fixtures);
    match set_paused(fake, repository, true) {
        response if response.status().is_success() => Json(Paused {
            stopped: if force { running } else { Vec::new() },
        })
        .into_response(),
        response => response,
    }
}

fn set_paused(fake: &Fake, repository: &str, paused: bool) -> Response {
    let mut fixtures = fake.fixtures.lock().unwrap();
    let now = fixtures.now;
    match fixtures
        .status
        .repositories
        .iter_mut()
        .find(|candidate| candidate.name == repository)
    {
        Some(found) => {
            found.paused = paused;
            // As the daemon announces it to every client.
            let _ = fake.events.send(EventEnvelope {
                id: None,
                at: now,
                event: Event::RepositoryChanged {
                    repository: found.clone(),
                },
            });
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found(&format!("{repository} is not a configured repository")),
    }
}

/// `POST /v1/repositories/pause` and `/resume`: every repository or, while any runs a job and without force, none.
fn set_all_paused(fake: &Fake, paused: bool, force: bool) -> Response {
    let fixtures = fake.fixtures.lock().unwrap();
    let names: Vec<String> = fixtures
        .status
        .repositories
        .iter()
        .map(|repository| repository.name.clone())
        .collect();
    let running: Vec<RunningJob> = names
        .iter()
        .flat_map(|name| running_jobs(&fixtures, name))
        .collect();
    let unrecorded = fixtures.unrecorded;
    drop(fixtures);
    let (verb, done, undone) = if paused {
        ("pause", "paused", "resumes it")
    } else {
        ("resume", "resumed", "pauses it again")
    };
    fake.fixtures
        .lock()
        .unwrap()
        .controls
        .push(format!("{verb} all force={force}"));
    if paused && !running.is_empty() && !force {
        let runs: Vec<String> = running
            .iter()
            .map(|job| format!("{} runs {job}", job.repository))
            .collect();
        let message = runs.join("; ");
        return (
            StatusCode::CONFLICT,
            Json(Busy {
                message,
                jobs: running,
            }),
        )
            .into_response();
    }
    for name in &names {
        set_paused(fake, name, paused);
    }
    if unrecorded {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                message: format!(
                    "every repository is {done} until runnerd restarts, but the job history cannot record it, so a restart {undone}"
                ),
            }),
        )
            .into_response();
    }
    if paused {
        Json(Paused {
            stopped: if force { running } else { Vec::new() },
        })
        .into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

/// Removes the records `chosen` picks, noting it on their jobs as the daemon's history does.
fn remove_records(fixtures: &mut Fixtures, chosen: impl Fn(&Record) -> bool) -> RemovedRecords {
    let now = fixtures.now;
    let (gone, kept) = std::mem::take(&mut fixtures.records.records)
        .into_iter()
        .partition::<Vec<_>, _>(|record| chosen(record));
    fixtures.records.records = kept;
    for record in &gone {
        if let Some(note) = fixtures
            .jobs
            .iter_mut()
            .find(|job| job.summary.id == record.job)
            .and_then(|job| job.record.as_mut())
        {
            note.removed_at = Some(now);
            note.removed_by = Some(RecordRemoval::User);
        }
    }
    RemovedRecords {
        jobs: gone.iter().map(|record| record.job).collect(),
        bytes: gone.iter().map(|record| record.bytes).sum(),
    }
}

fn router(fake: Fake) -> Router {
    Router::new()
        .route(
            "/v1/status",
            get(|State(fake): State<Fake>| async move { Json(fake.fixtures.lock().unwrap().status.clone()) }),
        )
        .route(
            "/v1/repositories/{owner}/{name}/pause",
            post(
                |State(fake): State<Fake>,
                 UrlPath((owner, name)): UrlPath<(String, String)>,
                 Query(query): Query<PauseQuery>| async move {
                    pause(&fake, &format!("{owner}/{name}"), query.force)
                },
            ),
        )
        .route(
            "/v1/repositories/pause",
            post(|State(fake): State<Fake>, Query(query): Query<PauseQuery>| async move {
                set_all_paused(&fake, true, query.force)
            }),
        )
        .route(
            "/v1/repositories/resume",
            post(|State(fake): State<Fake>| async move { set_all_paused(&fake, false, false) }),
        )
        .route(
            "/v1/repositories/{owner}/{name}/resume",
            post(|State(fake): State<Fake>, UrlPath((owner, name)): UrlPath<(String, String)>| async move {
                set_paused(&fake, &format!("{owner}/{name}"), false)
            }),
        )
        .route(
            "/v1/jobs",
            get(|State(fake): State<Fake>, Query(query): Query<JobQuery>| async move {
                Json(fake.fixtures.lock().unwrap().page(&query))
            }),
        )
        .route(
            "/v1/jobs/{id}",
            get(|State(fake): State<Fake>, UrlPath(id): UrlPath<u64>| async move {
                find_job(&fake, id).map_or_else(|| not_found("no such job"), |job| Json(job).into_response())
            }),
        )
        .route(
            "/v1/jobs/{id}/samples",
            get(|State(fake): State<Fake>, UrlPath(id): UrlPath<u64>| async move {
                let fixtures = fake.fixtures.lock().unwrap();
                Json(SampleSeries {
                    job: id,
                    interval: 2,
                    samples: fixtures.samples.get(&id).cloned().unwrap_or_default(),
                })
            }),
        )
        .route(
            "/v1/jobs/{id}/logs",
            get(
                |State(fake): State<Fake>, UrlPath(id): UrlPath<u64>, Query(query): Query<LogQuery>| async move {
                    let fixtures = fake.fixtures.lock().unwrap();
                    let lines = fixtures.logs.get(&id).cloned().unwrap_or_default();
                    let from = usize::try_from(query.from.unwrap_or(0)).unwrap().min(lines.len());
                    lines[from..].iter().map(|line| line.clone() + "\n").collect::<String>()
                },
            ),
        )
        .route(
            "/v1/host/samples",
            get(|State(fake): State<Fake>| async move {
                Json(HostSamples {
                    interval: 2,
                    samples: fake.fixtures.lock().unwrap().host.clone(),
                })
            }),
        )
        .route(
            "/v1/activity",
            get(|State(fake): State<Fake>, Query(query): Query<ActivityQuery>| async move {
                Json(fake.fixtures.lock().unwrap().activity(query.since))
            }),
        )
        .route(
            "/v1/stats",
            get(|State(fake): State<Fake>| async move {
                Json(Stats {
                    bucket: Bucket::Day,
                    rows: fake.fixtures.lock().unwrap().stats.clone(),
                })
            }),
        )
        .route(
            "/v1/caches",
            get(|State(fake): State<Fake>| async move { Json(fake.fixtures.lock().unwrap().caches.clone()) }),
        )
        .route(
            "/v1/caches/history",
            get(|State(fake): State<Fake>| async move {
                Json(CacheHistory {
                    sizes: fake.fixtures.lock().unwrap().cache_history.clone(),
                })
            }),
        )
        .route(
            "/v1/profiles/{name}/reseed",
            post(|State(fake): State<Fake>, UrlPath(name): UrlPath<String>| async move {
                fake.fixtures.lock().unwrap().controls.push(format!("reseed {name}"));
                StatusCode::ACCEPTED
            }),
        )
        .route(
            "/v1/caches/{name}/clean",
            post(|State(fake): State<Fake>, UrlPath(name): UrlPath<String>| async move {
                fake.fixtures.lock().unwrap().controls.push(format!("clean {name}"));
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/v1/records",
            get(|State(fake): State<Fake>| async move { Json(fake.fixtures.lock().unwrap().records.clone()) })
                .delete(|State(fake): State<Fake>, Query(query): Query<RemoveRecordsQuery>| async move {
                    let mut fixtures = fake.fixtures.lock().unwrap();
                    fixtures.controls.push(format!("remove records all={}", query.all));
                    Json(remove_records(&mut fixtures, |_| query.all))
                }),
        )
        .route(
            "/v1/records/{job}",
            delete(|State(fake): State<Fake>, UrlPath(job): UrlPath<u64>| async move {
                let mut fixtures = fake.fixtures.lock().unwrap();
                fixtures.controls.push(format!("remove record {job}"));
                if !fixtures.records.records.iter().any(|record| record.job == job) {
                    return not_found(&format!("job {job} has no record")).into_response();
                }
                Json(remove_records(&mut fixtures, |record| record.job == job)).into_response()
            }),
        )
        .route(
            "/v1/config",
            get(|State(fake): State<Fake>| async move {
                let text = fake.fixtures.lock().unwrap().config.clone();
                Json(ConfigFile {
                    path: "/home/forge/.config/runnerd/runnerd.toml".to_owned(),
                    revision: runnerd_core::config::revision(&text),
                    text,
                })
            })
            .put(|State(fake): State<Fake>, Query(query): Query<ReplaceQuery>, text: String| async move {
                let now = runnerd_core::config::revision(&fake.fixtures.lock().unwrap().config);
                if query.revision.is_some_and(|read| read != now) {
                    return (
                        StatusCode::CONFLICT,
                        Json(ErrorResponse { message: "the config changed since it was read".to_owned() }),
                    )
                        .into_response();
                }
                let (config, mut checked) = check(&text);
                if config.is_none() {
                    return (StatusCode::UNPROCESSABLE_ENTITY, Json(checked)).into_response();
                }
                checked.revision = Some(runnerd_core::config::revision(&text));
                let mut fixtures = fake.fixtures.lock().unwrap();
                fixtures.config = fixtures.after_put.take().unwrap_or(text);
                Json(checked).into_response()
            }),
        )
        .route("/v1/config/check", post(|text: String| async move { Json(check(&text).1) }))
        .route(
            "/v1/insights",
            get(|State(fake): State<Fake>| async move { Json(insights(&fake.fixtures.lock().unwrap())) }),
        )
        .route(
            "/v1/savings",
            get(|State(fake): State<Fake>| async move { Json(savings(&fake.fixtures.lock().unwrap())) }),
        )
        .route(
            "/v1/digest",
            get(|State(fake): State<Fake>| async move { Json(digest(&fake.fixtures.lock().unwrap())) }),
        )
        .route(
            "/v1/sizing",
            get(|State(fake): State<Fake>| async move { Json(sizing(&fake.fixtures.lock().unwrap())) }),
        )
        .route(
            "/v1/doctor",
            get(|| async {
                Json(Doctor {
                    checks: vec![
                        Check { name: "Docker".to_owned(), status: CheckStatus::Pass, detail: "29.1.3, reachable".to_owned(), fix: None },
                        Check { name: "sysbox".to_owned(), status: CheckStatus::Pass, detail: "sysbox-runc registered".to_owned(), fix: None },
                        Check {
                            name: "slice".to_owned(),
                            status: CheckStatus::Warn,
                            detail: "runnerd.slice has MemoryMax=33G but no MemoryHigh".to_owned(),
                            fix: Some("systemctl edit runnerd.slice, then add MemoryHigh=30G under [Slice]".to_owned()),
                        },
                    ],
                })
            }),
        )
        .route("/v1/events", get(events))
        .with_state(fake)
}

/// The events the caller feeds, filtered by the query as the daemon does.
async fn events(
    State(fake): State<Fake>,
    Query(query): Query<EventQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let receiver = fake.events.subscribe();
    let stream = stream::unfold((receiver, query), |(mut receiver, query)| async move {
        loop {
            match receiver.recv().await {
                Ok(envelope) if !query.matches(&envelope.event) => {}
                Ok(envelope) => {
                    let event = SseEvent::default()
                        .event(envelope.event.kind())
                        .data(serde_json::to_string(&envelope).unwrap());
                    return Some((Ok(event), (receiver, query)));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Serves the fake daemon on a new socket at `path` until the runtime stops.
pub fn serve(path: &Path, fake: Fake) {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(fake)).await.unwrap();
    });
}
