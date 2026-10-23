//! The API over a real Unix socket: routes, owner-only access, socket takeover, the event stream resumed across a
//! restart, a followed log, and the control routes' answers and config file over a fake engine.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use runnerd_core::api::{
    Budget, Busy, ConfigCheck, ConfigFile, ErrorResponse, Event, EventEnvelope, JobState, Paused,
    ProfileStatus, RunningJob, Status,
};
use runnerd_core::config::{ByteSize, Config};
use runnerd_core::github::{JobDetails, QueuedJob};
use runnerd_core::scheduler::engine::{
    Command, Commands, Control, ControlError, Handle, RunningJobs, Shutdown, Target,
};
use runnerd_core::store::{LogPosition, RunnerRecord, Store};
use tokio::sync::{mpsc, oneshot, watch};

use crate::testdir::Dir;

use super::{ConfigSource, SyncDir, bind, serve, serve_for, sync_dir};

const EXAMPLE: &str = include_str!("../../../../examples/runnerd.toml");

/// The slice's `MemoryMax`, in place of systemd's: the example's caps sum to 32.5G.
#[allow(
    clippy::unnecessary_wraps,
    reason = "it stands in for config::slice_memory_max"
)]
fn memory_max(_slice: &str) -> std::io::Result<Option<ByteSize>> {
    Ok(Some(ByteSize(33 << 30)))
}

/// A response's body.
async fn body(response: reqwest::Response) -> Vec<u8> {
    response.bytes().await.unwrap().to_vec()
}

/// The runner `o/busy` runs job 7 on, in the fake engine.
fn busy_job() -> RunningJob {
    RunningJob {
        repository: "o/busy".to_owned(),
        runner: "runner-1".to_owned(),
        job: Some(7),
    }
}

/// Answers commands as an engine whose repository `o/busy` runs job 7, `o/app` nothing, and no other is configured; sends every config it
/// is told to run with to `reloads`.
async fn fake_engine(
    control: Arc<Control>,
    mut commands: Commands,
    reloads: mpsc::UnboundedSender<Config>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            Command::Pause {
                target,
                jobs,
                reply,
            } => {
                let busy = match &target {
                    Target::Repository(repository) => repository == "o/busy",
                    Target::All => true,
                };
                let _ = reply.send(if busy && jobs == RunningJobs::Keep {
                    Err(ControlError::Busy {
                        message: "o/busy runs job 7 on runner-1".to_owned(),
                        jobs: vec![busy_job()],
                    })
                } else {
                    // Refuses a repository the config does not have.
                    control.set_paused(&target, true).await.map(|()| Paused {
                        stopped: if busy { vec![busy_job()] } else { Vec::new() },
                    })
                });
            }
            Command::Resume { target, reply } => {
                let _ = reply.send(control.set_paused(&target, false).await);
            }
            Command::Reseed { reply, .. } => {
                let _ = reply.send(Err(ControlError::Conflict("reseeding already".to_owned())));
            }
            Command::CleanCache { reply, .. } => {
                let _ = reply.send(Ok(()));
            }
            Command::ReplaceConfig {
                config,
                commit,
                reply,
                ..
            } => {
                let answer = commit.run().await;
                if answer.is_ok() {
                    let _ = reloads.send(*config);
                }
                let _ = reply.send(answer.map_err(ControlError::Failed));
            }
        }
    }
}

fn status() -> Status {
    Status {
        version: "0.1.0".to_owned(),
        shutting_down: false,
        budget: Budget {
            slice: "ci.slice".to_owned(),
            memory_max: Some(33 << 30),
            memory_in_use: 0,
        },
        profiles: vec![ProfileStatus {
            name: "sandbox".to_owned(),
            image: "runnerd-runner:base".to_owned(),
            cpus: 2,
            memory: 4 << 30,
            reseeding: false,
            slots: Vec::new(),
        }],
        repositories: Vec::new(),
    }
}

/// Serves the API on a socket in a fresh directory; returns a client bound to it and the controls behind it.
async fn daemon() -> (reqwest::Client, Arc<Control>, Dir) {
    daemon_for(None).await
}

/// `daemon`, serving only the user `owner` when given, instead of the user the test runs as.
async fn daemon_for(owner: Option<u32>) -> (reqwest::Client, Arc<Control>, Dir) {
    let dir = Dir::new();
    let daemon = start(&dir.0, owner).await;
    // Serves until the test ends.
    std::mem::forget(daemon.stop);
    (daemon.client, daemon.control, dir)
}

/// A daemon serving the history in `dir`, until `stop` is sent or dropped.
struct Daemon {
    client: reqwest::Client,
    control: Arc<Control>,
    store: Store,
    /// `runnerd.toml`, a symlink to the file it serves.
    config: std::path::PathBuf,
    reloads: mpsc::UnboundedReceiver<Config>,
    stop: oneshot::Sender<()>,
    served: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// Stops serving and waits until the server is done, as `runnerd` does on shutdown.
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(5), self.served)
            .await
            .unwrap()
            .unwrap();
        self.store.flush().await;
    }
}

async fn start(dir: &Path, owner: Option<u32>) -> Daemon {
    start_with(dir, owner, true, sync_dir).await
}

/// Fails as flushing a directory does on an I/O error.
fn failing_sync(_dir: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("input/output error"))
}

/// `start`; without `engine`, nothing answers the engine's commands, as once it stopped. Directories are flushed
/// with `sync_dir`.
async fn start_with(dir: &Path, owner: Option<u32>, engine: bool, sync_dir: SyncDir) -> Daemon {
    let socket = dir.join("runnerd.sock");
    let store = Store::open(&dir.join("history.db")).unwrap();
    let (control, commands) =
        Control::new(["o/app".to_owned(), "o/busy".to_owned()], [], store.clone());
    let control = Arc::new(control);
    let (reloaded, reloads) = mpsc::unbounded_channel();
    if engine {
        tokio::spawn(fake_engine(control.clone(), commands, reloaded));
    } else {
        drop(commands);
    }
    let (sender, receiver) = watch::channel(status());
    let handle = Handle {
        status: receiver,
        control: control.clone(),
        store: store.clone(),
        caches: watch::channel(Vec::new()).1,
    };
    let config = dir.join("runnerd.toml");
    let file = dir.join("config.toml");
    std::fs::write(&file, EXAMPLE).unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
    if !config.exists() {
        std::os::unix::fs::symlink(&file, &config).unwrap();
    }
    let source = ConfigSource {
        path: config.clone(),
        text: EXAMPLE.to_owned(),
        memory_max,
        sync_dir,
    };
    let listener = bind(&socket).await.unwrap();
    let mode = std::fs::metadata(&socket).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o600
    );
    let (stop, stopped) = oneshot::channel::<()>();
    let stopped = async {
        let _ = stopped.await;
    };
    let served = tokio::spawn(async move {
        let _sender = sender;
        match owner {
            Some(owner) => serve_for(listener, handle, source, owner, stopped).await,
            None => serve(listener, handle, source, stopped).await,
        }
        .unwrap();
    });
    runnerd_core::github::install_crypto_provider();
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap();
    Daemon {
        client,
        control,
        store,
        config,
        reloads,
        stop,
        served,
    }
}

#[tokio::test]
async fn status_lists_the_configured_profiles() {
    let (client, _, _dir) = daemon().await;
    let response = client.get("http://runnerd/v1/status").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Status = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body, status());
}

#[tokio::test]
async fn repositories_pause_and_resume_by_owner_and_name() {
    let (client, control, _dir) = daemon().await;
    let post = async |path: &str| {
        client
            .post(format!("http://runnerd{path}"))
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(post("/v1/repositories/o/app/pause").await, StatusCode::OK);
    assert!(control.is_paused("o/app"));
    assert_eq!(
        post("/v1/repositories/o/app/resume").await,
        StatusCode::NO_CONTENT
    );
    assert!(!control.is_paused("o/app"));
    assert_eq!(
        post("/v1/repositories/o/other/pause").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn every_repository_pauses_at_once_refused_while_one_runs_a_job_unless_forced_and_resumes_at_once()
 {
    let (client, control, _dir) = daemon().await;
    let refused = client
        .post("http://runnerd/v1/repositories/pause")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let busy: Busy = serde_json::from_slice(&body(refused).await).unwrap();
    assert_eq!(busy.jobs, [busy_job()]);
    assert!(!control.is_paused("o/app"), "a refused pause pauses none");

    let forced = client
        .post("http://runnerd/v1/repositories/pause?force=true")
        .send()
        .await
        .unwrap();
    assert_eq!(forced.status(), StatusCode::OK);
    let paused: Paused = serde_json::from_slice(&body(forced).await).unwrap();
    assert_eq!(paused.stopped, [busy_job()]);
    assert!(control.is_paused("o/app"), "o/app is paused");
    assert!(control.is_paused("o/busy"), "o/busy is paused");

    let resumed = client
        .post("http://runnerd/v1/repositories/resume")
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::NO_CONTENT);
    assert!(!control.is_paused("o/app"), "o/app runs again");
    assert!(!control.is_paused("o/busy"), "o/busy runs again");
}

#[tokio::test]
async fn a_pause_the_history_cannot_record_fails_saying_a_restart_undoes_it() {
    let (client, control, dir) = daemon().await;
    // As a full disk would.
    let fault = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_insert BEFORE INSERT ON paused BEGIN SELECT RAISE(ABORT, 'full'); END;",
        )
        .unwrap();
    let response = client
        .post("http://runnerd/v1/repositories/o/app/pause")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error: ErrorResponse = serde_json::from_slice(&body(response).await).unwrap();
    assert!(
        error.message.contains("a restart resumes it"),
        "{}",
        error.message
    );
    assert!(
        control.is_paused("o/app"),
        "o/app is paused until a restart"
    );
}

#[tokio::test]
async fn shutdown_is_graceful_unless_forced() {
    let (client, control, _dir) = daemon().await;
    let response = client
        .post("http://runnerd/v1/shutdown")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(control.mode(), Shutdown::Graceful);
    client
        .post("http://runnerd/v1/shutdown")
        .header("content-type", "application/json")
        .body(r#"{"force":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(control.mode(), Shutdown::Force);
}

#[tokio::test]
async fn every_route_refuses_other_users() {
    let me = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").unwrap());
    let (client, control, _dir) = daemon_for(Some(me + 1)).await;
    let requests = [
        client.get("http://runnerd/v1/status"),
        client.post("http://runnerd/v1/repositories/o/app/pause"),
        client.post("http://runnerd/v1/repositories/o/app/resume"),
        client.post("http://runnerd/v1/shutdown"),
        client.get("http://runnerd/v1/jobs"),
        client.get("http://runnerd/v1/jobs/1"),
        client.get("http://runnerd/v1/jobs/1/samples"),
        client.get("http://runnerd/v1/jobs/1/logs"),
        client.get("http://runnerd/v1/host/samples"),
        client.get("http://runnerd/v1/stats"),
        client.get("http://runnerd/v1/caches"),
        client.get("http://runnerd/v1/caches/history"),
        client.get("http://runnerd/v1/events"),
        client.post("http://runnerd/v1/repositories/o/busy/pause?force=true"),
        client.post("http://runnerd/v1/repositories/pause?force=true"),
        client.post("http://runnerd/v1/repositories/resume"),
        client.post("http://runnerd/v1/profiles/jvm/reseed"),
        client.post("http://runnerd/v1/caches/runnerd-pnpm/clean"),
        client.get("http://runnerd/v1/config"),
        client.post("http://runnerd/v1/config/check").body(EXAMPLE),
        client.put("http://runnerd/v1/config").body(EXAMPLE),
        client.get("http://runnerd/v1/doctor"),
    ];
    for request in requests {
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{}",
            response.url()
        );
    }
    assert!(!control.is_paused("o/app"));
    assert!(!control.is_paused("o/busy"));
    assert_eq!(control.mode(), Shutdown::No);
}

#[tokio::test]
async fn a_pause_while_a_job_runs_is_refused_naming_the_job_unless_forced() {
    let (client, control, _dir) = daemon().await;
    let response = client
        .post("http://runnerd/v1/repositories/o/busy/pause")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let busy: Busy = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(busy.jobs, [busy_job()]);
    assert!(busy.message.contains("job 7"), "{}", busy.message);
    assert!(!control.is_paused("o/busy"));
    let response = client
        .post("http://runnerd/v1/repositories/o/busy/pause?force=true")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let paused: Paused = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(paused.stopped, [busy_job()]);
    assert!(control.is_paused("o/busy"));
}

#[tokio::test]
async fn a_config_over_the_budget_is_refused_with_both_numbers_and_the_file_is_unchanged() {
    let dir = Dir::new();
    let mut daemon = start(&dir.0, None).await;
    let before = std::fs::read(&daemon.config).unwrap();
    // A third jvm slot: 3 × 9G + 2 × 7G + 512M = 41.5G.
    let over = EXAMPLE.replacen("slots = 2", "slots = 3", 1);
    let checked: ConfigCheck = serde_json::from_slice(
        &body(
            daemon
                .client
                .post("http://runnerd/v1/config/check")
                .body(over.clone())
                .send()
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert!(!checked.valid);
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(over)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let refused: ConfigCheck = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(refused, checked);
    assert!(
        refused.message.contains("41.5G") && refused.message.contains("33G"),
        "{}",
        refused.message
    );
    assert_eq!(std::fs::read(&daemon.config).unwrap(), before);
    assert!(daemon.reloads.try_recv().is_err());
    let shown: ConfigFile = serde_json::from_slice(
        &body(
            daemon
                .client
                .get("http://runnerd/v1/config")
                .send()
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(shown.text, EXAMPLE);
    daemon.stop().await;
}

#[tokio::test]
async fn a_config_the_engine_cannot_take_leaves_the_file_unchanged() {
    let dir = Dir::new();
    let daemon = start_with(&dir.0, None, false, sync_dir).await;
    let before = std::fs::read_to_string(&daemon.config).unwrap();
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(EXAMPLE.replacen("slots = 2", "slots = 1", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), before);
    let shown: ConfigFile = serde_json::from_slice(
        &body(
            daemon
                .client
                .get("http://runnerd/v1/config")
                .send()
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(shown.text, EXAMPLE);
    daemon.stop().await;
}

#[tokio::test]
async fn a_config_whose_file_cannot_be_written_is_refused_and_not_run() {
    let dir = Dir::new();
    let mut daemon = start(&dir.0, None).await;
    let set_mode = |mode| {
        std::fs::set_permissions(&dir.0, std::os::unix::fs::PermissionsExt::from_mode(mode))
            .unwrap();
    };
    // No new file can be created next to the config.
    set_mode(0o500);
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(EXAMPLE.replacen("slots = 2", "slots = 1", 1))
        .send()
        .await
        .unwrap();
    set_mode(0o700);
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), EXAMPLE);
    assert!(daemon.reloads.try_recv().is_err());
    let shown: ConfigFile = serde_json::from_slice(
        &body(
            daemon
                .client
                .get("http://runnerd/v1/config")
                .send()
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(shown.text, EXAMPLE);
    daemon.stop().await;
}

#[tokio::test]
async fn a_config_in_place_whose_directory_cannot_be_flushed_is_run_and_shown_with_a_warning() {
    let dir = Dir::new();
    let mut daemon = start_with(&dir.0, None, true, failing_sync).await;
    let new = EXAMPLE.replacen("slots = 2", "slots = 1", 1);
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(new.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let checked: ConfigCheck = serde_json::from_slice(&body(response).await).unwrap();
    assert!(
        checked
            .warnings
            .iter()
            .any(|warning| warning.contains("input/output error")),
        "{:?}",
        checked.warnings
    );
    // The file, the engine and what the API shows agree.
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), new);
    assert_eq!(
        daemon.reloads.recv().await.unwrap().profiles["jvm"].slots,
        1
    );
    let shown: ConfigFile = serde_json::from_slice(
        &body(
            daemon
                .client
                .get("http://runnerd/v1/config")
                .send()
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(shown.text, new);
    daemon.stop().await;
}

#[tokio::test]
async fn a_valid_config_replaces_the_file_it_links_to_with_its_permissions_and_is_reloaded() {
    let dir = Dir::new();
    let mut daemon = start(&dir.0, None).await;
    let new = EXAMPLE.replacen("slots = 2", "slots = 1", 1);
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(new.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let checked: ConfigCheck = serde_json::from_slice(&body(response).await).unwrap();
    assert!(checked.valid, "{}", checked.message);
    assert!(
        std::fs::symlink_metadata(&daemon.config)
            .unwrap()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), new);
    let mode = std::fs::metadata(&daemon.config).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o640
    );
    let reloaded = daemon.reloads.recv().await.unwrap();
    assert_eq!(reloaded.profiles["jvm"].slots, 1);
    let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        // Temporary files start with a dot.
        .filter(|name| name.starts_with('.'))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new());
    daemon.stop().await;
}

#[tokio::test]
async fn a_second_daemon_does_not_take_over_a_live_socket() {
    let (_client, _, dir) = daemon().await;
    assert!(bind(&dir.0.join("runnerd.sock")).await.is_err());
}

fn budget(memory_in_use: u64) -> Event {
    Event::BudgetChanged {
        budget: Budget {
            slice: "ci.slice".to_owned(),
            memory_max: None,
            memory_in_use,
        },
    }
}

/// Reads `count` events from an SSE response.
async fn read_events(response: &mut reqwest::Response, count: usize) -> Vec<EventEnvelope> {
    let mut text = String::new();
    let mut events = Vec::new();
    while events.len() < count {
        let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .expect("no event in 5s")
            .unwrap()
            .expect("the stream ended");
        text.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = text.find("\n\n") {
            let block: String = text.drain(..end + 2).collect();
            if let Some(data) = block.lines().find_map(|line| line.strip_prefix("data: ")) {
                events.push(serde_json::from_str(data).unwrap());
            }
        }
    }
    events
}

fn memory(events: &[EventEnvelope]) -> Vec<u64> {
    events
        .iter()
        .map(|event| match &event.event {
            Event::BudgetChanged { budget } => budget.memory_in_use,
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn after_a_restart_the_event_stream_resumes_after_the_last_id_without_duplicates() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    daemon.store.event(budget(1));
    daemon.store.event(budget(2));
    daemon.store.flush().await;
    let mut response = daemon
        .client
        .get("http://runnerd/v1/events")
        .header("last-event-id", "0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let first = read_events(&mut response, 2).await;
    daemon.store.event(budget(3));
    let live = read_events(&mut response, 1).await;
    assert_eq!(memory(&first), [1, 2]);
    assert_eq!(memory(&live), [3]);
    // The client saw event 2 last before the restart.
    let last = first[1].id.unwrap();
    drop(response);
    daemon.stop().await;

    let daemon = start(&dir.0, None).await;
    daemon.store.event(budget(4));
    daemon.store.flush().await;
    let mut response = daemon
        .client
        .get("http://runnerd/v1/events")
        .header("last-event-id", last.to_string())
        .send()
        .await
        .unwrap();
    let mut resumed = read_events(&mut response, 2).await;
    daemon.store.event(budget(5));
    resumed.extend(read_events(&mut response, 1).await);
    assert_eq!(memory(&resumed), [3, 4, 5]);
    let ids: Vec<u64> = resumed.iter().filter_map(|event| event.id).collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
    assert!(ids[0] > last);
    daemon.stop().await;
}

#[tokio::test]
async fn a_followed_log_sends_stored_and_new_lines_and_ends_when_the_job_finishes() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let store = &daemon.store;
    store.queued(
        "o/app",
        &QueuedJob {
            id: 1,
            run_id: 2,
            name: "build".to_owned(),
            ..QueuedJob::default()
        },
    );
    store.runner_started(RunnerRecord {
        name: "runner-1".to_owned(),
        repository: "o/app".to_owned(),
        profile: "app".to_owned(),
        slot: 1,
        container: "c1".to_owned(),
        started_at: 1_000,
        memory_cap: Some(4 << 30),
        cpu_cap: Some(2000),
    });
    store.claimed(1, 2, "runner-1", 2_000);
    let position = LogPosition { page: 1, offset: 0 };
    store
        .log(
            1,
            vec!["one".to_owned(), "two".to_owned()],
            position,
            1 << 20,
        )
        .await;
    store.flush().await;
    let mut response = daemon
        .client
        .get("http://runnerd/v1/jobs/1/logs?follow=true&from=1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = String::new();
    while body != "two\n" {
        body.push_str(&String::from_utf8_lossy(
            &response.chunk().await.unwrap().unwrap(),
        ));
    }
    store
        .log(1, vec!["three".to_owned()], position, 1 << 20)
        .await;
    store.details(JobDetails {
        id: 1,
        run_id: 2,
        run_attempt: 1,
        name: "build".to_owned(),
        workflow_name: None,
        head_branch: None,
        html_url: None,
        labels: Vec::new(),
        created_at: None,
        status: "completed".to_owned(),
        conclusion: Some("success".to_owned()),
        started_at: None,
        completed_at: None,
        runner_name: Some("runner-1".to_owned()),
        steps: Vec::new(),
    });
    store.runner_removed("runner-1", 3_000);
    let rest = tokio::time::timeout(Duration::from_secs(5), response.text())
        .await
        .expect("the log did not end with its job")
        .unwrap();
    assert_eq!(body + &rest, "two\nthree\n");
    let job = store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Completed);
    daemon.stop().await;
}
