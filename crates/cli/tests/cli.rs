//! `runnerctl` against a fake daemon on a temporary Unix socket: what it prints, what it asks the daemon, and how
//! it reports a daemon that is not there or refuses.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use runnerd_core::api::{
    Advice, Budget, Busy, Check, CheckStatus, ConfigCheck, ConfigFile, Doctor, ErrorResponse,
    Event, EventEnvelope, Job, JobMetrics, JobRunner, JobSizing, JobState, JobSummary, LogSummary,
    Pairing, Pairings, Paused, Peaks, ProfileSizing, ProfileStatus, QueuedJob, Record,
    RecordLimits, Records, RemovedRecords, RepositoryStatus, Resize, RunnerStatus, RunningJob,
    Sizing, SizingChange, SlotStatus, Spread, Status, Timeline, Waiting,
};

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "runnerctl-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn status() -> Status {
    Status {
        version: "0.0.1".to_owned(),
        shutting_down: false,
        budget: Budget {
            admission: runnerd_core::api::AdmissionPolicy::Caps,
            held: false,
            slice: "runnerd.slice".to_owned(),
            memory_max: Some(16 << 30),
            memory_in_use: 6 << 30,
            mirror_memory: 0,
            seed_memory: Vec::new(),
            shell_memory: Vec::new(),
            slice_cpus: None,
            cpu: Some(runnerd_core::api::CpuBudget {
                cpus: 32_000,
                budget: 32_000,
                in_use: 4500,
                held: true,
            }),
        },
        profiles: vec![ProfileStatus {
            name: "jvm".to_owned(),
            image: "runnerd-runner:jvm".to_owned(),
            cpus: 4,
            memory: 6 << 30,
            reseeding: false,
            slots: vec![
                SlotStatus {
                    slot: 1,
                    runner: Some(RunnerStatus {
                        repository: "halyard/ledger".to_owned(),
                        name: "runnerd-5f2a-jvm-1-k3j9".to_owned(),
                        runner_id: 81,
                        container: "4e1d".to_owned(),
                        busy: true,
                        started_at: 1_700_000_000,
                        reserved: 9 << 30,
                        warm: false,
                    }),
                },
                SlotStatus {
                    slot: 2,
                    runner: None,
                },
            ],
        }],
        repositories: vec![RepositoryStatus {
            name: "halyard/ledger".to_owned(),
            profile: "jvm".to_owned(),
            labels: vec!["self-hosted".to_owned(), "jvm".to_owned()],
            paused: false,
            queued_jobs: 1,
        }],
        queued: vec![QueuedJob {
            id: 9002,
            repository: "halyard/ledger".to_owned(),
            run_id: 78,
            run_attempt: 1,
            workflow: Some("build".to_owned()),
            name: "lint".to_owned(),
            branch: Some("main".to_owned()),
            url: None,
            queued_at: 1_000_000,
            waiting: Some(Waiting::NoSlot {
                profile: "jvm".to_owned(),
            }),
        }],
    }
}

fn job(state: JobState) -> Job {
    Job {
        admission: None,
        summary: JobSummary {
            id: 9001,
            repository: "halyard/ledger".to_owned(),
            run_id: 77,
            run_attempt: 1,
            workflow: Some("build".to_owned()),
            name: "test".to_owned(),
            branch: Some("main".to_owned()),
            url: None,
            labels: vec!["self-hosted".to_owned()],
            state,
            conclusion: state.is_final().then(|| "success".to_owned()),
            runner: Some(JobRunner {
                name: "runnerd-5f2a-jvm-1-k3j9".to_owned(),
                profile: "jvm".to_owned(),
                slot: 1,
                memory_cap: Some(6 << 30),
                cpu_cap: Some(4000),
            }),
            timeline: Timeline {
                queued_at: 1_700_000_000,
                started_at: Some(1_700_000_004),
                ..Timeline::default()
            },
            metrics: JobMetrics::default(),
            log: LogSummary::default(),
        },
        steps: Vec::new(),
        record: None,
    }
}

/// What the fake daemon was asked, and what it answers next.
#[derive(Default)]
struct Fake {
    /// The query string of every log request.
    log_queries: Vec<String>,
    /// The `Last-Event-ID` of every event stream request.
    event_ids: Vec<Option<String>>,
    /// Every request but log and status reads, in order, as `METHOD path`.
    requests: Vec<String>,
    /// The body of every config check.
    checked: Vec<String>,
    /// The body of every config replacement.
    replaced: Vec<String>,
    /// The query of every config replacement.
    replaced_queries: Vec<Option<String>>,
}

type Shared = Arc<Mutex<Fake>>;

async fn get_status() -> axum::Json<Status> {
    axum::Json(status())
}

async fn get_job(State(fake): State<Shared>) -> axum::Json<Job> {
    // Final once the client asked for the log twice: the first followed response ended early, as on a restart.
    let final_now = fake.lock().unwrap().log_queries.len() >= 2;
    axum::Json(job(if final_now {
        JobState::Completed
    } else {
        JobState::Running
    }))
}

async fn get_logs(State(fake): State<Shared>, RawQuery(query): RawQuery) -> String {
    let mut fake = fake.lock().unwrap();
    fake.log_queries.push(query.unwrap_or_default());
    if fake.log_queries.len() == 1 {
        "Run ./gradlew test\nBUILD SUCCESSFUL\n".to_owned()
    } else {
        "Cleaning up\n".to_owned()
    }
}

/// `halyard/ledger` runs a job: a pause without force is refused, a forced one stops it. `halyard/wharf`'s pause
/// holds, but the daemon's history cannot record it.
async fn pause(
    UrlPath((owner, name)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    paused(&format!("{owner}/{name}"), query.as_deref())
}

/// A pause of every repository: `halyard/ledger`'s job is in the way, as for its own pause.
async fn pause_all(RawQuery(query): RawQuery) -> Response {
    paused("halyard/ledger", query.as_deref())
}

fn paused(repository: &str, query: Option<&str>) -> Response {
    let running = RunningJob {
        repository: repository.to_owned(),
        runner: "runnerd-5f2a-jvm-1-k3j9".to_owned(),
        job: Some(9001),
    };
    match (repository, query) {
        ("halyard/ledger", Some("force=true")) => axum::Json(Paused {
            stopped: vec![running],
        })
        .into_response(),
        ("halyard/wharf", _) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(ErrorResponse {
                message: "halyard/wharf is paused until runnerd restarts, but the job history cannot record it, \
                          so a restart resumes it"
                    .to_owned(),
            }),
        )
            .into_response(),
        ("halyard/ledger", _) => (
            StatusCode::CONFLICT,
            axum::Json(Busy {
                message: format!("{repository} runs {running}"),
                jobs: vec![running],
            }),
        )
            .into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorResponse {
                message: format!("{repository} is not a configured repository"),
            }),
        )
            .into_response(),
    }
}

async fn reseed(State(fake): State<Shared>, UrlPath(profile): UrlPath<String>) -> StatusCode {
    fake.lock()
        .unwrap()
        .requests
        .push(format!("POST /v1/profiles/{profile}/reseed"));
    StatusCode::ACCEPTED
}

async fn check_config(State(fake): State<Shared>, body: String) -> axum::Json<ConfigCheck> {
    fake.lock().unwrap().checked.push(body);
    axum::Json(ConfigCheck {
        valid: false,
        message: "memory caps sum to 20G, more than runnerd.slice's MemoryMax of 16G".to_owned(),
        warnings: Vec::new(),
        memory_caps: Some(20 << 30),
        memory_max: Some(16 << 30),
        revision: None,
    })
}

/// The daemon's config: `node` 2 × 7G, with a comment that must survive an edit.
const CONFIG: &str = "[profiles.node]\nimage_stage = \"node\"\ncpus = 8\nmemory = \"7G\" # peaked at 6G once\nslots = 2\n";

async fn get_config() -> axum::Json<ConfigFile> {
    axum::Json(ConfigFile {
        path: "/home/u/.config/runnerd/runnerd.toml".to_owned(),
        text: CONFIG.to_owned(),
        revision: Some("c0ffee".to_owned()),
    })
}

async fn replace_config(
    State(fake): State<Shared>,
    RawQuery(query): RawQuery,
    body: String,
) -> axum::Json<ConfigCheck> {
    let mut fake = fake.lock().unwrap();
    fake.replaced.push(body);
    fake.replaced_queries.push(query);
    axum::Json(ConfigCheck {
        valid: true,
        message: "valid, memory caps sum to 13G".to_owned(),
        warnings: Vec::new(),
        memory_caps: Some(13 << 30),
        memory_max: Some(16 << 30),
        revision: None,
    })
}

/// `node`'s 100 builds peak at 2.1G: 2.5G with the margin, 5 of them in the 14G its 2 slots take. Auto sizing,
/// since turned off, lowered it from 9G.
async fn get_sizing() -> axum::Json<Sizing> {
    let spread = |p50, p95, p99, max| Spread { p50, p95, p99, max };
    let peaks = Peaks {
        jobs: 100,
        at_cap: 0,
        memory: spread(1500 << 20, 2 << 30, 2150 << 20, 2200 << 20),
        cpu: spread(3000, 6000, 7500, 8000),
    };
    let advice = Advice::Lower {
        memory: 2560 << 20,
        slots: 5,
    };
    axum::Json(Sizing {
        auto_plan: None,
        auto: false,
        changes: vec![SizingChange {
            at: 1_700_050_000,
            profile: "node".to_owned(),
            from: Resize {
                memory: 9 << 30,
                slots: 2,
                cpus: Some(6),
            },
            to: Resize {
                memory: 7 << 30,
                slots: 2,
                cpus: Some(8),
            },
            reason: "lower memory to 2.5G from 9G, its largest peak + 15%".to_owned(),
            uncertain: false,
        }],
        since: 1_700_000_000,
        until: 1_700_086_400,
        margin: 15,
        sample_interval: 2,
        min_jobs: 10,
        memory_max: Some(16 << 30),
        admission: runnerd_core::api::AdmissionPolicy::Caps,
        profiles: vec![ProfileSizing {
            name: "node".to_owned(),
            memory: 7 << 30,
            cpus: 8,
            slots: 2,
            peaks: peaks.clone(),
            wait: spread(30, 240, 300, 320),
            busy: 80,
            wants_slots: true,
            advice,
            jobs: vec![JobSizing {
                repository: "halyard/storefront".to_owned(),
                workflow: Some("CI".to_owned()),
                name: "build".to_owned(),
                peaks,
                wait: spread(30, 240, 300, 320),
                advice,
            }],
        }],
    })
}

async fn get_doctor() -> axum::Json<Doctor> {
    axum::Json(Doctor {
        checks: vec![
            Check {
                name: "Docker".to_owned(),
                status: CheckStatus::Pass,
                detail: "29.1.0".to_owned(),
                fix: None,
            },
            Check {
                name: "sysbox".to_owned(),
                status: CheckStatus::Fail,
                detail: "sysbox-runc is not a Docker runtime".to_owned(),
                fix: Some("install sysbox".to_owned()),
            },
        ],
    })
}

fn sse(envelope: &EventEnvelope) -> String {
    let id = envelope
        .id
        .map(|id| format!("id: {id}\n"))
        .unwrap_or_default();
    format!(
        "event: {}\n{id}data: {}\n\n",
        envelope.event.kind(),
        serde_json::to_string(envelope).unwrap()
    )
}

async fn events(State(fake): State<Shared>, headers: HeaderMap) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.requests.push("GET /v1/events".to_owned());
    fake.event_ids.push(
        headers
            .get("last-event-id")
            .map(|value| value.to_str().unwrap().to_owned()),
    );
    // The first stream sends two events and ends, as for a client that fell behind; later ones end at once.
    let body = if fake.event_ids.len() == 1 {
        let mut summary = job(JobState::Running).summary;
        summary.state = JobState::Running;
        format!(
            ": keep-alive\n\n{}{}{}{}",
            sse(&EventEnvelope {
                id: Some(41),
                at: 1_700_000_004,
                event: Event::JobStarted { job: summary },
            }),
            sse(&EventEnvelope {
                id: None,
                at: 1_700_000_006,
                event: Event::LogLine {
                    job: 9001,
                    line: 0,
                    text: "live only".to_owned()
                },
            }),
            sse(&EventEnvelope {
                id: Some(42),
                at: 1_700_000_007,
                event: Event::CacheReseeded {
                    profile: "node".to_owned(),
                    output: "node: slot 1 changed 3.0G".to_owned(),
                    error: None,
                },
            }),
            sse(&EventEnvelope {
                id: Some(43),
                at: 1_700_000_008,
                event: Event::CacheReseeded {
                    profile: "jvm".to_owned(),
                    output: "jvm: slot 1 changed 2.1G".to_owned(),
                    error: None,
                },
            }),
        )
    } else {
        String::new()
    };
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

/// Fresh links on each of two `[web] origins`, or on the one asked for.
async fn pairings(RawQuery(query): RawQuery) -> Response {
    static MADE: AtomicU32 = AtomicU32::new(0);
    let origins = match query.as_deref() {
        None => vec!["https://forge.example.ts.net:8443", "https://forge.lan"],
        Some("origin=https%3A%2F%2Fforge.lan") => vec!["https://forge.lan"],
        Some(_) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "https://other.example is not one of [web] origins",
            );
        }
    };
    let links = origins
        .into_iter()
        .map(|origin| Pairing {
            origin: origin.to_owned(),
            url: format!(
                "{origin}/login?code={:064x}",
                MADE.fetch_add(1, Ordering::Relaxed)
            ),
            lifetime: 300,
        })
        .collect();
    axum::Json(Pairings { links }).into_response()
}

/// Serves the fake daemon on `socket` until the test process ends.
fn serve(socket: &Path) -> Shared {
    let fake = Shared::default();
    let app = Router::new()
        .route("/v1/status", get(get_status))
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/jobs/{id}/logs", get(get_logs))
        .route("/v1/repositories/{owner}/{name}/pause", post(pause))
        .route("/v1/repositories/pause", post(pause_all))
        .route(
            "/v1/repositories/resume",
            post(async || StatusCode::NO_CONTENT),
        )
        .route("/v1/profiles/{name}/reseed", post(reseed))
        .route("/v1/config/check", post(check_config))
        .route("/v1/config", get(get_config).put(replace_config))
        .route("/v1/sizing", get(get_sizing))
        .route("/v1/doctor", get(get_doctor))
        .route("/v1/events", get(events))
        .route("/v1/records", get(get_records))
        .route("/v1/records", delete(remove_records))
        .route("/v1/records/{job}", delete(remove_record))
        .route("/v1/dashboard/pairings", post(pairings))
        // `[web] open` names the second proxy of `LOGIN_LINKS`.
        .route(
            "/v1/dashboard",
            get(async || axum::Json(serde_json::json!({ "open": "https://forge.lan" }))),
        )
        .route(
            "/v1/records/{job}/shell",
            post(async || {
                refuse(
                    StatusCode::CONFLICT,
                    "the budget is full: 8G of the slice's 8G is held",
                )
            }),
        )
        .with_state(fake.clone());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(async { tokio::net::UnixListener::bind(socket) })
        .unwrap();
    std::thread::spawn(move || {
        runtime.block_on(async { axum::serve(listener, app).await.unwrap() });
    });
    fake
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(ErrorResponse {
            message: message.to_owned(),
        }),
    )
        .into_response()
}

async fn get_records() -> axum::Json<Records> {
    let record = |job: u64, gib: u64| Record {
        job,
        repository: "halyard/ledger".to_owned(),
        profile: "app".to_owned(),
        workflow: Some("build".to_owned()),
        name: Some("test".to_owned()),
        created_at: 1_792_000_000,
        bytes: gib << 30,
        image: format!("runnerd-record:{job}"),
    };
    axum::Json(Records {
        records: vec![record(9001, 1), record(9002, 3)],
        limits: RecordLimits {
            max_total: 20 << 30,
            max_count: 10,
            max_age: 7 * 86_400,
            min_free: 10 << 30,
        },
        shells: vec![9002],
    })
}

async fn remove_records(
    State(fake): State<Shared>,
    RawQuery(query): RawQuery,
) -> axum::Json<RemovedRecords> {
    fake.lock()
        .unwrap()
        .requests
        .push(format!("DELETE /v1/records?{}", query.unwrap_or_default()));
    axum::Json(RemovedRecords {
        jobs: vec![9001],
        bytes: 1 << 30,
    })
}

async fn remove_record(UrlPath(job): UrlPath<u64>) -> Response {
    refuse(
        StatusCode::CONFLICT,
        &format!("a shell is open in the record of job {job}; remove it once the shell exits"),
    )
}

/// Runs `runnerctl --socket socket args`, with stdout a pipe, failing the test after 10 s.
fn runnerctl(socket: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_runnerctl"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .env_remove("NO_COLOR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("runnerctl {args:?} did not finish within 10 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn the_version_is_reported_under_the_command_name() {
    // The package is runnerd-cli; what people type and read is runnerctl.
    let output = Command::new(env!("CARGO_BIN_EXE_runnerctl"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("runnerctl {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn the_login_url_goes_through_the_proxy_named_by_base() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    std::fs::write(
        dir.0.join("runnerd.web"),
        "http://127.0.0.1:7477/login?code=0123abcd\n",
    )
    .unwrap();
    let output = runnerctl(
        &socket,
        &[
            "web",
            "--no-open",
            "--base",
            "https://Forge.example.ts.net:8443",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "https://forge.example.ts.net:8443/login?code=0123abcd\n"
    );
    let output = runnerctl(
        &socket,
        &[
            "web",
            "--no-open",
            "--base",
            "https://forge.example.ts.net:8443/",
        ],
    );
    assert!(!output.status.success(), "a base with a path is refused");
    assert!(
        stderr(&output).contains("it has a path"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn status_json_is_the_api_type_unchanged() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let output = runnerctl(&socket, &["status", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let printed: Status = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(printed, status());
}

#[test]
fn status_piped_to_another_program_is_aligned_text_without_colour() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let output = runnerctl(&socket, &["status"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(!text.contains('\x1b'), "{text:?}");
    assert!(
        text.contains("6G of 16G reserved in runnerd.slice"),
        "{text}"
    );
    assert!(
        text.contains("CPU 4.5 of 32 CPUs reserved; a job waits for CPU"),
        "{text}"
    );
    let slots: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("jvm"))
        .collect();
    assert_eq!(slots.len(), 2, "{text}");
    assert_eq!(
        slots[0].find("busy"),
        slots[1].find("free"),
        "the state column lines up:\n{text}"
    );
    let status = status();
    let reason = Waiting::NoSlot {
        profile: "jvm".to_owned(),
    }
    .reason(&status.queued[0], &status, 0);
    assert!(
        text.lines().any(
            |line| line.starts_with("9002  halyard/ledger  build / lint  ")
                && line.ends_with(&format!(" ago  {reason}"))
        ),
        "the queued job is listed under the repositories with why it waits, as every client says it:\n{text}"
    );
}

#[test]
fn a_followed_log_resumes_after_the_last_line_and_stops_once_the_job_is_final() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let fake = serve(&socket);
    let output = runnerctl(&socket, &["logs", "9001", "-f"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "Run ./gradlew test\nBUILD SUCCESSFUL\nCleaning up\n"
    );
    assert_eq!(
        fake.lock().unwrap().log_queries,
        ["from=0&follow=true", "from=2&follow=true"]
    );
}

#[test]
fn watch_resumes_after_the_last_durable_event_when_the_stream_ends() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let fake = serve(&socket);
    let output = runnerctl(&socket, &["watch", "--json"]);
    // The second stream ends at once too, so watch gives up.
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("keeps closing the event stream"),
        "{}",
        stderr(&output)
    );
    let printed: Vec<EventEnvelope> = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(printed.len(), 4);
    assert_eq!(printed[0].id, Some(41));
    assert_eq!(
        fake.lock().unwrap().event_ids,
        [None, Some("43".to_owned())]
    );
}

#[test]
fn with_no_daemon_it_says_runnerd_is_not_running_and_how_to_start_it() {
    let dir = TempDir::new();
    let output = runnerctl(&dir.0.join("runnerd.sock"), &["status"]);
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("runnerd is not running"), "{error}");
    assert!(error.contains("systemctl --user start runnerd"), "{error}");
}

#[test]
fn the_daemons_error_message_reaches_the_user() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let output = runnerctl(&socket, &["pause", "halyard/ledgr"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("halyard/ledgr is not a configured repository"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_pause_while_the_repository_runs_a_job_names_the_job_and_how_to_force_it() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let refused = runnerctl(&socket, &["pause", "halyard/ledger"]);
    assert!(!refused.status.success());
    let error = stderr(&refused);
    assert!(
        error.contains("job 9001 on runnerd-5f2a-jvm-1-k3j9"),
        "{error}"
    );
    assert!(
        error.contains("runnerctl pause --force halyard/ledger"),
        "{error}"
    );

    let forced = runnerctl(&socket, &["pause", "--force", "halyard/ledger"]);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(
        stdout(&forced).contains("stopped job 9001 on runnerd-5f2a-jvm-1-k3j9"),
        "{}",
        stdout(&forced)
    );
}

#[test]
fn a_pause_the_daemon_cannot_record_fails_saying_a_restart_resumes_it() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let output = runnerctl(&socket, &["pause", "halyard/wharf"]);
    assert!(
        !output.status.success(),
        "a pause that is not durable fails"
    );
    assert!(
        stderr(&output).contains("a restart resumes it"),
        "{}",
        stderr(&output)
    );
    assert!(!stdout(&output).contains("paused:"), "{}", stdout(&output));
}

#[test]
fn pause_all_is_refused_while_a_job_runs_says_how_to_force_it_and_resume_all_resumes() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let refused = runnerctl(&socket, &["pause", "--all"]);
    assert!(!refused.status.success(), "a pause while a job runs fails");
    let error = stderr(&refused);
    assert!(error.contains("job 9001"), "{error}");
    assert!(error.contains("runnerctl pause --force --all"), "{error}");

    let forced = runnerctl(&socket, &["pause", "--all", "--force"]);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(
        stdout(&forced).contains("every repository paused"),
        "{}",
        stdout(&forced)
    );

    let resumed = runnerctl(&socket, &["resume", "--all"]);
    assert!(resumed.status.success(), "{}", stderr(&resumed));
    assert_eq!(stdout(&resumed), "every repository resumed\n");
}

#[test]
fn reseed_listens_before_asking_and_waits_for_its_own_profile() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let fake = serve(&socket);
    let output = runnerctl(&socket, &["reseed", "jvm"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("jvm: slot 1 changed 2.1G\njvm: reseeded"),
        "{text}"
    );
    assert!(!text.contains("node"), "{text}");
    assert_eq!(
        fake.lock().unwrap().requests,
        ["GET /v1/events", "POST /v1/profiles/jvm/reseed"]
    );
}

#[test]
fn a_config_over_the_budget_fails_the_check_with_the_daemons_reason() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let fake = serve(&socket);
    let file = dir.0.join("runnerd.toml");
    std::fs::write(&file, "[host]\nslice = \"runnerd.slice\"\n").unwrap();
    let output = runnerctl(&socket, &["config", "check", file.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        stdout(&output).contains(
            "runnerd.toml: memory caps sum to 20G, more than runnerd.slice's MemoryMax of 16G"
        ),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        fake.lock().unwrap().checked,
        ["[host]\nslice = \"runnerd.slice\"\n"]
    );
}

#[test]
fn doctor_prints_each_check_as_runnerd_does_and_fails_when_one_fails() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let output = runnerctl(&socket, &["doctor"]);
    assert!(!output.status.success());
    assert_eq!(
        stdout(&output),
        "ok    Docker: 29.1.0\nFAIL  sysbox: sysbox-runc is not a Docker runtime\n      fix: install sysbox\n"
    );
}

#[test]
fn web_prints_the_login_url_the_daemon_wrote_next_to_its_socket() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let missing = runnerctl(&socket, &["web", "--no-open"]);
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("runnerd is not running, or was started with --web off"),
        "{}",
        stderr(&missing)
    );
    let url = "http://127.0.0.1:7477/login?code=0f3a";
    std::fs::write(dir.0.join("runnerd.web"), format!("{url}\n")).unwrap();
    let output = runnerctl(&socket, &["web", "--no-open"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{url}\n"));
}

const LOGIN_LINKS: &str = "http://127.0.0.1:7477/login?code=aa\n\
                           https://forge.example.ts.net:8443/login?code=bb\n\
                           https://forge.lan/login?code=cc\n";

#[test]
fn web_prints_its_own_link_and_one_for_each_configured_proxy() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    std::fs::write(dir.0.join("runnerd.web"), LOGIN_LINKS).unwrap();
    let output = runnerctl(&socket, &["web", "--no-open"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), LOGIN_LINKS);
    // A proxy in `[web] origins` gets its own link; any other the loopback one's code.
    for (base, link) in [
        ("https://forge.lan", "https://forge.lan/login?code=cc"),
        (
            "https://other.example",
            "https://other.example/login?code=aa",
        ),
    ] {
        let output = runnerctl(&socket, &["web", "--no-open", "--base", base]);
        assert_eq!(stdout(&output), format!("{link}\n"), "--base {base}");
    }
}

#[test]
fn web_opens_the_origin_web_open_names_unless_told_otherwise_for_one_run() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let _fake = serve(&socket);
    std::fs::write(dir.0.join("runnerd.web"), LOGIN_LINKS).unwrap();
    let links = |args: &[&str]| {
        let output = runnerctl(&socket, &[&["web", "--no-open"], args].concat());
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    };
    assert_eq!(
        links(&[]),
        "https://forge.lan/login?code=cc\n\
         http://127.0.0.1:7477/login?code=aa\n\
         https://forge.example.ts.net:8443/login?code=bb\n",
        "the tailnet's link first, the loopback one still printed"
    );
    assert_eq!(links(&["--local"]), LOGIN_LINKS, "--local opens loopback");
    assert_eq!(
        links(&["--base", "https://forge.example.ts.net:8443"]),
        "https://forge.example.ts.net:8443/login?code=bb\n"
    );
    let both = runnerctl(&socket, &["web", "--local", "--base", "https://forge.lan"]);
    assert!(!both.status.success(), "--local and --base conflict");
}

#[test]
fn web_qr_shows_a_fresh_code_for_each_proxy_each_time_and_relays_a_refusal() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let first = runnerctl(&socket, &["web", "--qr"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let shown = stdout(&first);
    for origin in ["https://forge.example.ts.net:8443", "https://forge.lan"] {
        assert!(
            shown.contains(&format!(
                "on {origin}, within 5 minutes: {origin}/login?code="
            )),
            "{shown}"
        );
    }
    assert!(shown.contains('█'), "QR codes:\n{shown}");
    let again = stdout(&runnerctl(&socket, &["web", "--qr"]));
    let links = |text: &str| -> Vec<String> {
        text.lines()
            .filter_map(|line| line.split_once(": ").map(|(_, link)| link.to_owned()))
            .collect()
    };
    assert!(
        links(&shown)
            .iter()
            .all(|link| !links(&again).contains(link)),
        "each run shows new codes"
    );
    let one = stdout(&runnerctl(
        &socket,
        &["web", "--qr", "--base", "https://forge.lan"],
    ));
    assert_eq!(links(&one).len(), 1, "{one}");
    let refused = runnerctl(&socket, &["web", "--qr", "--base", "https://other.example"]);
    assert!(!refused.status.success(), "an unlisted origin gets no link");
    assert!(
        stderr(&refused).contains("is not one of [web] origins"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn a_job_shows_the_caps_its_container_ran_with() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let output = runnerctl(&socket, &["job", "9001"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("(profile jvm, slot 1, capped at 6G and 4 CPUs)"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn records_list_their_sizes_and_limits_and_go_by_age_or_one_by_one() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let fake = serve(&socket);

    let listed = runnerctl(&socket, &["records"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let text = stdout(&listed);
    assert!(
        text.starts_with("2 records, 4G of 20G; at most 10, each for 7d"),
        "{text}"
    );
    assert!(text.contains("9002"), "{text}");
    assert!(text.contains("shell open"), "{text}");

    let removed = runnerctl(&socket, &["records", "rm", "--older-than", "2d"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert_eq!(stdout(&removed), "removed 1 record, 1G freed\n");
    assert_eq!(
        fake.lock().unwrap().requests,
        ["DELETE /v1/records?all=false&older_than=2d"]
    );

    let refused = runnerctl(&socket, &["records", "rm", "9002"]);
    assert!(
        !refused.status.success(),
        "a record with a shell open stays"
    );
    assert!(
        stderr(&refused).contains("a shell is open in the record of job 9002"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn a_shell_the_budget_has_no_room_for_says_so() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let refused = runnerctl(&socket, &["shell", "9001"]);
    assert!(!refused.status.success(), "the shell is refused");
    assert!(
        stderr(&refused).contains("the budget is full"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn sizing_shows_the_advice_and_applies_it_only_when_told_to() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let fake = serve(&socket);
    let shown = runnerctl(&socket, &["sizing"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    let text = stdout(&shown);
    assert!(
        text.contains("node  7G × 2, 8 CPUs; 100 jobs, slots busy 80%"),
        "{text}"
    );
    assert!(
        text.contains(
            "lower memory to 2.5G from 7G, its largest peak + 15%; 5 slots fit in the same 14G"
        ),
        "{text}"
    );
    assert!(text.contains("runnerctl sizing apply node"), "{text}");
    assert!(text.contains("halyard/storefront  CI / build"), "{text}");

    // No terminal to ask on: nothing is written.
    let asked = runnerctl(&socket, &["sizing", "apply", "node"]);
    assert!(!asked.status.success());
    assert!(stderr(&asked).contains("pass --yes"), "{}", stderr(&asked));
    assert!(
        fake.lock().unwrap().replaced.is_empty(),
        "nothing is written without a yes"
    );

    let applied = runnerctl(&socket, &["sizing", "apply", "node", "--yes"]);
    assert!(applied.status.success(), "{}", stderr(&applied));
    assert!(
        stdout(&applied).contains("node: memory 2.5G, 5 slots written"),
        "{}",
        stdout(&applied)
    );
    let replaced = fake.lock().unwrap().replaced.clone();
    assert_eq!(
        replaced,
        [CONFIG
            .replace("memory = \"7G\"", "memory = \"2.5G\"")
            .replace("slots = 2", "slots = 5")]
    );
}

#[test]
fn sizing_shows_what_auto_sizing_changed_and_undoes_it_back_to_what_it_replaced() {
    let dir = TempDir::new();
    let socket = dir.0.join("runnerd.sock");
    let fake = serve(&socket);
    let shown = runnerctl(&socket, &["sizing"]);
    let text = stdout(&shown);
    assert!(
        text.contains("memory 9G → 7G, slots 2, cpus 6 → 8"),
        "{text}"
    );
    assert!(text.contains("runnerctl sizing undo node"), "{text}");

    let undone = runnerctl(&socket, &["sizing", "undo", "node", "--yes"]);
    assert!(undone.status.success(), "{}", stderr(&undone));
    assert_eq!(
        fake.lock().unwrap().replaced_queries,
        [Some("revision=c0ffee".to_owned())],
        "the write is made against the config it read, so one replaced since is not overwritten"
    );
    let replaced = fake.lock().unwrap().replaced.clone();
    assert_eq!(
        replaced,
        [CONFIG
            .replace("memory = \"7G\"", "memory = \"9G\"")
            .replace("cpus = 8", "cpus = 6")],
        "memory and the cpus it changed are back to what it replaced"
    );
}
