//! The API over a real Unix socket: routes, owner-only access, socket takeover, the event stream resumed across a
//! restart, and a followed log.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use runnerd_core::api::{Budget, Event, EventEnvelope, JobState, ProfileStatus, Status};
use runnerd_core::github::{JobDetails, QueuedJob};
use runnerd_core::scheduler::engine::{Control, Handle, Shutdown};
use runnerd_core::store::{LogPosition, RunnerRecord, Store};
use tokio::sync::{oneshot, watch};

use super::{bind, serve, serve_for};

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
async fn daemon() -> (reqwest::Client, Arc<Control>, tempdir::Dir) {
    daemon_for(None).await
}

/// `daemon`, serving only the user `owner` when given, instead of the user the test runs as.
async fn daemon_for(owner: Option<u32>) -> (reqwest::Client, Arc<Control>, tempdir::Dir) {
    let dir = tempdir::Dir::new();
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
    let socket = dir.join("runnerd.sock");
    let store = Store::open(&dir.join("history.db")).unwrap();
    let control = Arc::new(Control::new(["o/app".to_owned()]));
    let (sender, receiver) = watch::channel(status());
    let handle = Handle {
        status: receiver,
        control: control.clone(),
        store: store.clone(),
        caches: Vec::new(),
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
            Some(owner) => serve_for(listener, handle, owner, stopped).await,
            None => serve(listener, handle, stopped).await,
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
    assert_eq!(
        post("/v1/repositories/o/app/pause").await,
        StatusCode::NO_CONTENT
    );
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
    assert_eq!(control.mode(), Shutdown::No);
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
    let dir = tempdir::Dir::new();
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
    let dir = tempdir::Dir::new();
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
    });
    store.claimed(1, 2, "runner-1", 2_000);
    let position = LogPosition { page: 1, offset: 0 };
    store.log(
        1,
        vec!["one".to_owned(), "two".to_owned()],
        position,
        1 << 20,
    );
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
    store.log(1, vec!["three".to_owned()], position, 1 << 20);
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

mod tempdir {
    use std::path::PathBuf;

    /// A directory under the system temp dir, removed on drop.
    pub(crate) struct Dir(pub PathBuf);

    impl Dir {
        pub(crate) fn new() -> Dir {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("runnerd-test-api-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
