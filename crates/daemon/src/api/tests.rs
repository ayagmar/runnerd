//! The API over a real Unix socket: routes, owner-only access, socket takeover, the event stream resumed across a
//! restart, a followed log, and the control routes' answers and config file over a fake engine.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use runnerd_core::api::{
    Advice, Budget, Busy, ConfigCheck, ConfigFile, ErrorResponse, Event, EventEnvelope, HostSample,
    HostSamples, JobState, Pairings, Paused, ProfileStatus, Records, RemovedRecords, Resize,
    RunningJob, Sizing, SizingChange, Status,
};
use runnerd_core::config::{ByteSize, Config};
use runnerd_core::docker::Tty;
use runnerd_core::github::{JobDetails, QueuedJob};
use runnerd_core::scheduler::engine::{
    Command, Commands, Control, ControlError, Handle, RecordSelection, RunningJobs, Shutdown,
    Target,
};
use runnerd_core::store::{LogPosition, RunnerRecord, SampleRecord, Store};
use tokio::sync::{mpsc, oneshot, watch};

use crate::testdir::Dir;

use super::notify::{Setup, Timing};
use super::{ConfigSource, MemoryMax, SyncDir, bind, serve, serve_for, sync_dir};

const EXAMPLE: &str = include_str!("../../../../examples/runnerd.toml");

/// The notifier at a test's pace, in UTC.
const NOTIFY: Setup = Setup {
    timing: Timing {
        gather: Duration::from_millis(50),
        gap: Duration::from_millis(50),
        retry: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        check: Duration::from_millis(100),
        timeout: Duration::from_secs(2),
    },
    offset: || Some(0),
};

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
/// A shell opens in job 9's record, whose shell prints a prompt and exits, and in job 10's, whose shell echoes its
/// input until it ends; the budget has no room for any other. Every shell closed is sent to `closed`.
async fn fake_engine(
    control: Arc<Control>,
    mut commands: Commands,
    reloads: mpsc::UnboundedSender<Config>,
    closed: mpsc::UnboundedSender<String>,
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
            Command::RemoveRecords { selection, reply } => {
                let _ = reply.send(match selection {
                    RecordSelection::Job(7) => Err(ControlError::Conflict(
                        "a shell is open in the record of job 7".to_owned(),
                    )),
                    _ => Ok(RemovedRecords {
                        jobs: vec![8],
                        bytes: 1 << 30,
                    }),
                });
            }
            Command::OpenShell { job, reply, .. } => {
                let prompt = futures_util::stream::iter([Ok(b"$ ".to_vec())]);
                let _ = reply.send(match job {
                    9 => Ok(Tty {
                        container: "shell-9".to_owned(),
                        output: prompt.boxed(),
                        input: Box::pin(tokio::io::sink()),
                    }),
                    10 => {
                        // Like bash at a terminal, it echoes what it reads and exits on Ctrl-D; its terminal's input
                        // closing does not end it.
                        let (input, reader) = tokio::io::duplex(64);
                        let echo = futures_util::stream::unfold(reader, |mut reader| async move {
                            let mut buffer = [0; 64];
                            match tokio::io::AsyncReadExt::read(&mut reader, &mut buffer).await {
                                Ok(read) if buffer[..read].contains(&0x04) => None,
                                Ok(read) if read > 0 => Some((Ok(buffer[..read].to_vec()), reader)),
                                _ => std::future::pending().await,
                            }
                        });
                        Ok(Tty {
                            container: "shell-10".to_owned(),
                            output: prompt.chain(echo).boxed(),
                            input: Box::pin(input),
                        })
                    }
                    _ => Err(ControlError::Conflict("the budget is full".to_owned())),
                });
            }
            Command::CloseShell { container } => {
                let _ = closed.send(container);
            }
        }
    }
}

fn status() -> Status {
    Status {
        version: "0.1.0".to_owned(),
        shutting_down: false,
        budget: Budget {
            admission: runnerd_core::api::AdmissionPolicy::Caps,
            held: false,
            slice: "ci.slice".to_owned(),
            memory_max: Some(33 << 30),
            memory_in_use: 0,
            mirror_memory: 0,
            seed_memory: Vec::new(),
            shell_memory: Vec::new(),
            slice_cpus: None,
            host: None,
            cpu: None,
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
        queued: Vec::new(),
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
    /// The shells the API closed.
    closed: mpsc::UnboundedReceiver<String>,
    stop: oneshot::Sender<()>,
    served: tokio::task::JoinHandle<()>,
    /// The dashboard's credentials, which a test's dashboard shares.
    sessions: Arc<runnerd_web::Sessions>,
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
    start_with(dir, owner, true, sync_dir, memory_max).await
}

/// Fails as `systemctl show` does for a slice whose CPUs it cannot report.
fn unreadable_cpus(_slice: &str) -> std::io::Result<u64> {
    Err(std::io::Error::other("systemctl show ci.slice failed"))
}

/// Fails as flushing a directory does on an I/O error.
fn failing_sync(_dir: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("input/output error"))
}

/// `start`; without `engine`, nothing answers the engine's commands, as once it stopped. Directories are flushed
/// with `sync_dir`.
async fn start_with(
    dir: &Path,
    owner: Option<u32>,
    engine: bool,
    sync_dir: SyncDir,
    memory_max: MemoryMax,
) -> Daemon {
    start_serving(dir, owner, engine, sync_dir, memory_max, EXAMPLE).await
}

/// `start_with`, serving the config `text`.
async fn start_serving(
    dir: &Path,
    owner: Option<u32>,
    engine: bool,
    sync_dir: SyncDir,
    memory_max: MemoryMax,
    text: &str,
) -> Daemon {
    let socket = dir.join("runnerd.sock");
    let store = Store::open(&dir.join("history.db")).unwrap();
    let (control, commands) =
        Control::new(["o/app".to_owned(), "o/busy".to_owned()], [], store.clone());
    let control = Arc::new(control);
    let (reloaded, reloads) = mpsc::unbounded_channel();
    let (closing, closed) = mpsc::unbounded_channel();
    if engine {
        tokio::spawn(fake_engine(control.clone(), commands, reloaded, closing));
    } else {
        drop(commands);
    }
    let (sender, receiver) = watch::channel(status());
    let handle = Handle {
        status: receiver,
        control: control.clone(),
        store: store.clone(),
        caches: watch::channel(Vec::new()).1,
        records: watch::channel(Records::default()).1,
        build_cache: None,
    };
    let config = dir.join("runnerd.toml");
    let file = dir.join("config.toml");
    std::fs::write(&file, text).unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
    if !config.exists() {
        std::os::unix::fs::symlink(&file, &config).unwrap();
    }
    let source = ConfigSource {
        path: config.clone(),
        text: text.to_owned(),
        memory_max,
        cpus: unreadable_cpus,
        sync_dir,
        auto_every: Duration::from_millis(20),
        notify: NOTIFY,
    };
    let sessions = runnerd_web::Sessions::open(dir.join("dashboard-session")).unwrap();
    let sessions_kept = sessions.clone();
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
            Some(owner) => serve_for(listener, handle, source, sessions, owner, stopped).await,
            None => serve(listener, handle, source, sessions, stopped).await,
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
        closed,
        stop,
        served,
        sessions: sessions_kept,
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
async fn sizing_advises_on_every_profile_of_the_config_within_the_slices_budget() {
    let (client, _, _dir) = daemon().await;
    let response = client.get("http://runnerd/v1/sizing").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let sizing: Sizing = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let profiles: Vec<_> = sizing
        .profiles
        .iter()
        .map(|profile| (profile.name.as_str(), profile.memory, profile.advice))
        .collect();
    assert_eq!(
        profiles,
        [
            ("jvm", 9 << 30, Advice::TooFew { jobs: 0 }),
            ("node", 7 << 30, Advice::TooFew { jobs: 0 })
        ]
    );
    assert_eq!(
        (sizing.memory_max, sizing.margin, sizing.min_jobs),
        (Some(33 << 30), 15, 10)
    );
}

/// The slice's `MemoryMax` in GiB for `rising_memory_max`, which one test raises as `systemctl daemon-reload` would.
static SLICE_GIB: AtomicU64 = AtomicU64::new(24);

#[allow(
    clippy::unnecessary_wraps,
    reason = "it stands in for config::slice_memory_max"
)]
fn rising_memory_max(_slice: &str) -> std::io::Result<Option<ByteSize>> {
    Ok(Some(ByteSize(SLICE_GIB.load(Ordering::SeqCst) << 30)))
}

#[tokio::test]
async fn a_raised_memory_max_counts_in_the_sizing_advice_without_a_restart() {
    let dir = Dir::new();
    let daemon = start_with(&dir.0, None, true, sync_dir, rising_memory_max).await;
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    // Ten jvm jobs at their 9G cap: they need 10.5G a slot.
    for id in 1..=10 {
        let runner = format!("runner-{id}");
        daemon.store.queued(
            "o/app",
            &QueuedJob {
                id,
                run_id: 2,
                name: "test".to_owned(),
                created_at: now - 60_000,
                ..QueuedJob::default()
            },
        );
        daemon.store.runner_started(RunnerRecord {
            admission: None,
            name: runner.clone(),
            repository: "o/app".to_owned(),
            profile: "jvm".to_owned(),
            slot: 1,
            container: format!("c{id}"),
            started_at: now - 50_000,
            memory_cap: Some(9 << 30),
            cpu_cap: Some(8000),
        });
        daemon.store.claimed(id, 2, &runner, now - 40_000);
        // The container's own 9G limit stopped an allocation.
        daemon.store.samples(vec![SampleRecord {
            events: runnerd_core::observe::metrics::Events {
                oom: Some(1),
                local_oom: Some(1),
                ..runnerd_core::observe::metrics::Events::default()
            },
            runner: runner.clone(),
            at: now - 30_000,
            cpu: 0,
            cpu_usage: 0,
            memory: 9 << 30,
            network_received: 0,
            network_sent: 0,
        }]);
        daemon.store.lost(id, now - 20_000);
    }
    daemon.store.flush().await;
    let jvm = async || -> Advice {
        let response = daemon
            .client
            .get("http://runnerd/v1/sizing")
            .send()
            .await
            .unwrap();
        let sizing: Sizing = serde_json::from_slice(&body(response).await).unwrap();
        sizing
            .profiles
            .iter()
            .find(|p| p.name == "jvm")
            .unwrap()
            .advice
    };
    // 24G less node's 14G and the mirror's 0.5G leaves 9.5G: not one slot.
    assert_eq!(
        jvm().await,
        Advice::SliceTooSmall {
            memory: 10752 << 20
        }
    );
    SLICE_GIB.store(36, Ordering::SeqCst);
    assert_eq!(
        jvm().await,
        Advice::Raise {
            memory: 10752 << 20,
            slots: 2
        }
    );
    daemon.stop().await;
}

/// Records ten finished `o/app` jobs of `profile` at a `cap`, each peaking at `peak`, lost 20 s ago.
async fn finished_jobs(store: &Store, profile: &str, cap: u64, peak: u64) {
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    finished_jobs_at(store, profile, cap, peak, now).await;
}

/// [`finished_jobs`], lost 20 s before `now` (ms).
async fn finished_jobs_at(store: &Store, profile: &str, cap: u64, peak: u64, now: u64) {
    for id in 1..=10 {
        let runner = format!("runner-{id}");
        store.queued(
            "o/app",
            &QueuedJob {
                id,
                run_id: 2,
                name: "build".to_owned(),
                created_at: now - 60_000,
                ..QueuedJob::default()
            },
        );
        store.runner_started(RunnerRecord {
            admission: None,
            name: runner.clone(),
            repository: "o/app".to_owned(),
            profile: profile.to_owned(),
            slot: 1,
            container: format!("c{id}"),
            started_at: now - 50_000,
            memory_cap: Some(cap),
            cpu_cap: Some(8000),
        });
        store.claimed(id, 2, &runner, now - 40_000);
        store.samples(vec![SampleRecord {
            events: runnerd_core::observe::metrics::Events::default(),
            runner: runner.clone(),
            at: now - 30_000,
            cpu: 6000,
            cpu_usage: 0,
            memory: peak,
            network_received: 0,
            network_sent: 0,
        }]);
        store.lost(id, now - 20_000);
    }
    store.flush().await;
}

#[tokio::test]
async fn the_dashboard_says_which_origin_runnerctl_web_opens_as_the_daemon_started_with_it() {
    let dashboard = async |text: &str| -> runnerd_core::api::Dashboard {
        let dir = Dir::new();
        let daemon = start_serving(&dir.0, None, false, sync_dir, memory_max, text).await;
        let response = daemon
            .client
            .get("http://runnerd/v1/dashboard")
            .send()
            .await
            .unwrap();
        let dashboard = serde_json::from_slice(&body(response).await).unwrap();
        daemon.stop().await;
        dashboard
    };
    assert_eq!(dashboard(EXAMPLE).await.open, None, "loopback unless asked");
    let tailnet = format!(
        "{EXAMPLE}\n[web]\norigins = [\"https://forge.tail0b5e.ts.net:8443\"]\nopen = \"https://forge.tail0b5e.ts.net:8443\"\n"
    );
    assert_eq!(
        dashboard(&tailnet).await.open.as_deref(),
        Some("https://forge.tail0b5e.ts.net:8443")
    );
}

#[tokio::test]
async fn activity_counts_the_jobs_started_and_ended_in_the_last_24_hours() {
    let dir = Dir::new();
    let daemon = start_serving(&dir.0, None, false, sync_dir, memory_max, &auto_sizing()).await;
    // Ten jobs that waited 20 s, started 40 s ago and were lost 20 s ago.
    finished_jobs(&daemon.store, "node", 7 << 30, 2 << 30).await;
    let response = daemon
        .client
        .get("http://runnerd/v1/activity")
        .send()
        .await
        .unwrap();
    let activity: runnerd_core::api::Activity =
        serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(activity.until - activity.since, 24 * 3600);
    assert_eq!(activity.started.len(), 10);
    assert!(
        activity.started.iter().all(|job| job.waited == 20),
        "each waited 20 s: {:?}",
        activity.started
    );
    assert_eq!((activity.finished, activity.failed), (10, 10));
    daemon.stop().await;
}

#[tokio::test]
async fn auto_sizing_writes_advice_into_the_file_keeping_its_comments_and_waits_the_interval() {
    let dir = Dir::new();
    let text = auto_sizing();
    let mut daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &text).await;
    let mut events = daemon
        .client
        .get("http://runnerd/v1/events")
        .send()
        .await
        .unwrap();
    // Ten node jobs at their 7G cap peaking at 2G: lowered, by at most 25% a change.
    finished_jobs(&daemon.store, "node", 7 << 30, 2 << 30).await;
    let changed = changes_made(&daemon, 1).await;
    assert!(changed.auto, "the advice says auto sizing is on");
    let resized = Resize {
        memory: 5376 << 20,
        slots: 2,
        cpus: None,
    };
    assert_eq!(
        changed
            .changes
            .iter()
            .map(|change| (change.profile.as_str(), change.from, change.to))
            .collect::<Vec<_>>(),
        vec![(
            "node",
            Resize {
                memory: 7 << 30,
                slots: 2,
                cpus: None
            },
            resized
        )]
    );
    let written = std::fs::read_to_string(&daemon.config).unwrap();
    let node = &Config::parse(&written).unwrap().profiles["node"];
    assert_eq!((node.memory.0, node.slots), (resized.memory, resized.slots));
    for comment in text
        .lines()
        .filter(|line| line.trim_start().starts_with('#'))
    {
        assert!(written.contains(comment), "the file keeps {comment:?}");
    }
    let reloaded = daemon.reloads.recv().await.unwrap();
    assert_eq!(reloaded.profiles["node"].memory.0, resized.memory);
    // Read until it, past the jobs' own events.
    let announced = loop {
        let [envelope] = &read_events(&mut events, 1).await[..] else {
            unreachable!("one event read")
        };
        if let Event::SizingChanged { change } = &envelope.event {
            break change.clone();
        }
    };
    assert_eq!(announced, changed.changes[0]);
    // Many looks later, the intervals still hold node where it is, and the plan says until when.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later = read_sizing(&daemon).await;
    assert_eq!(later.changes.len(), 1);
    let plan = later.auto_plan.expect("auto sizing says what it does next");
    assert_eq!(
        plan.next,
        Vec::new(),
        "node is held, so the next look changes nothing"
    );
    assert_eq!(
        plan.held
            .iter()
            .map(|hold| (
                hold.profile.as_str(),
                hold.until - hold.changed,
                hold.lower_until - hold.changed
            ))
            .collect::<Vec<_>>(),
        [("node", 3600, 24 * 3600)],
        "node may be raised after an hour and lowered after a day"
    );
    assert_eq!(
        plan.blocked, None,
        "the file runnerd wrote is the one it runs with"
    );
    assert!(daemon.reloads.try_recv().is_err(), "no second reload");
    // A build meets the lowered cap: the next look takes node back to 7G, without waiting the hour a raise waits.
    met_cap(&daemon.store, "node", resized.memory).await;
    let reverted = changes_made(&daemon, 2).await.changes[1].clone();
    assert_eq!(
        (reverted.from, reverted.to.memory, reverted.to.slots),
        (resized, 7 << 30, 2)
    );
    assert!(
        reverted
            .reason
            .starts_with("raise memory back to 7G: o/app build met the 5.25G cap"),
        "{}",
        reverted.reason
    );
    daemon.stop().await;
}

/// The advice once auto sizing has made `count` changes, within 10 s.
async fn changes_made(daemon: &Daemon, count: usize) -> Sizing {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sizing = read_sizing(daemon).await;
            if sizing.changes.len() >= count {
                return sizing;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("auto sizing makes its changes within 10 s")
}

/// `profile` run at `cap`, as the engine announces a reload, and one job of it lost just now after its runner's own
/// cap failed reclaim.
async fn met_cap(store: &Store, profile: &str, cap: u64) {
    store.event(Event::ProfileChanged {
        profile: ProfileStatus {
            name: profile.to_owned(),
            image: format!("runnerd-runner:{profile}"),
            cpus: 8,
            memory: cap,
            reseeding: false,
            slots: Vec::new(),
        },
    });
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    store.queued(
        "o/app",
        &QueuedJob {
            id: 11,
            run_id: 3,
            name: "build".to_owned(),
            created_at: now - 60_000,
            ..QueuedJob::default()
        },
    );
    store.runner_started(RunnerRecord {
        admission: None,
        name: "runner-11".to_owned(),
        repository: "o/app".to_owned(),
        profile: profile.to_owned(),
        slot: 1,
        container: "c11".to_owned(),
        started_at: now - 50_000,
        memory_cap: Some(cap),
        cpu_cap: Some(8000),
    });
    store.claimed(11, 3, "runner-11", now - 40_000);
    store.samples(vec![SampleRecord {
        events: runnerd_core::observe::metrics::Events {
            oom: Some(1),
            oom_kill: Some(1),
            sock_throttled: None,
            local_oom: Some(1),
        },
        runner: "runner-11".to_owned(),
        at: now - 30_000,
        cpu: 6000,
        cpu_usage: 0,
        memory: cap - (64 << 20),
        network_received: 0,
        network_sent: 0,
    }]);
    store.lost(11, now);
    store.flush().await;
}

/// The example with auto sizing on.
fn auto_sizing() -> String {
    format!(
        "{EXAMPLE}\n# Let runnerd follow the advice.\n[sizing]\nauto = true\n# The pace these tests were written for.\ninterval = \"24h\"\nmax_step = 25\n"
    )
}

/// The advice, with the changes auto sizing made.
async fn read_sizing(daemon: &Daemon) -> Sizing {
    let response = daemon
        .client
        .get("http://runnerd/v1/sizing")
        .send()
        .await
        .unwrap();
    serde_json::from_slice(&body(response).await).unwrap()
}

#[tokio::test]
async fn auto_sizing_leaves_a_file_edited_on_disk_until_runnerd_runs_with_it() {
    let dir = Dir::new();
    let mut daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &auto_sizing()).await;
    let edited = format!("{}\n# Edited by hand, not reloaded yet.\n", auto_sizing());
    std::fs::write(&daemon.config, &edited).unwrap();
    finished_jobs(&daemon.store, "node", 7 << 30, 2 << 30).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), edited);
    let sizing = read_sizing(&daemon).await;
    assert_eq!(sizing.changes, Vec::new());
    // What it would write once runnerd runs with the file.
    let plan = sizing
        .auto_plan
        .expect("auto sizing says what it does next");
    assert_eq!(
        plan.next
            .iter()
            .map(|change| (change.profile.as_str(), change.to.memory))
            .collect::<Vec<_>>(),
        [("node", 5376 << 20)]
    );
    assert_eq!((plan.interval, plan.max_step), (24 * 3600, 25));
    // But no look writes it: the forecast says so instead of promising it.
    let blocked = plan.blocked.expect("the edited file blocks every look");
    assert!(blocked.contains("changed on disk"), "{blocked}");
    assert!(daemon.reloads.try_recv().is_err(), "nothing reloaded");
    daemon.stop().await;
}

#[tokio::test]
async fn a_change_the_history_cannot_record_is_not_written() {
    let dir = Dir::new();
    let mut daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &auto_sizing()).await;
    let fault = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_insert BEFORE INSERT ON auto_sizing BEGIN SELECT RAISE(ABORT, 'full'); END;",
        )
        .unwrap();
    finished_jobs(&daemon.store, "node", 7 << 30, 2 << 30).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        std::fs::read_to_string(&daemon.config).unwrap(),
        auto_sizing()
    );
    assert!(daemon.reloads.try_recv().is_err(), "nothing reloaded");
    drop(fault);
    daemon.stop().await;
}

#[tokio::test]
async fn a_change_a_crash_left_pending_is_resolved_from_the_file_and_holds_its_interval() {
    let dir = Dir::new();
    // The file has node's change: the daemon stopped after writing it, before recording it as done.
    let written = auto_sizing().replacen("memory = \"7G\"", "memory = \"5.25G\"", 1);
    let mut daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &written).await;
    // One value, so its `at` can't move to the next second between the record and the assertion.
    let change = SizingChange {
        at: super::history::now(),
        profile: "node".to_owned(),
        from: Resize {
            memory: 7 << 30,
            slots: 2,
            cpus: None,
        },
        to: Resize {
            memory: 5376 << 20,
            slots: 2,
            cpus: None,
        },
        reason: "lower memory".to_owned(),
        uncertain: false,
    };
    daemon.store.sizing_intent(change.clone()).await.unwrap();
    finished_jobs(&daemon.store, "node", 5376 << 20, 2 << 30).await;
    let resolved = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sizing = read_sizing(&daemon).await;
            if !sizing.changes.is_empty() {
                return sizing.changes;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the pending change is resolved within 10 s");
    assert_eq!(resolved, vec![change]);
    assert_eq!(
        daemon.store.auto_sizing().await.unwrap().pending,
        Vec::new()
    );
    // node's peaks would lower it further, but its change holds the interval.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), written);
    assert!(daemon.reloads.try_recv().is_err(), "nothing reloaded");
    daemon.stop().await;
}

/// Waits until the history lists `count` changes auto sizing made.
async fn until_changes(daemon: &Daemon, count: usize) -> Vec<SizingChange> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let changes = read_sizing(daemon).await.changes;
            if changes.len() >= count {
                return changes;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the changes are listed within 10 s")
}

#[tokio::test]
async fn a_written_change_whose_record_failed_and_was_then_changed_by_hand_stays_uncertain_and_holds_its_interval()
 {
    let dir = Dir::new();
    let mut daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &auto_sizing()).await;
    // The history takes the pending record but not what became of it.
    let fault = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_update BEFORE UPDATE ON auto_sizing BEGIN SELECT RAISE(ABORT, 'full'); END;",
        )
        .unwrap();
    finished_jobs(&daemon.store, "node", 7 << 30, 2 << 30).await;
    let reloaded = daemon.reloads.recv().await.unwrap();
    assert_eq!(reloaded.profiles["node"].memory.0, 5376 << 20);
    // Before the history can settle it, a person sets node to 6G.
    let by_hand = std::fs::read_to_string(&daemon.config).unwrap().replacen(
        "memory = \"5.25G\"",
        "memory = \"6G\"",
        1,
    );
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(by_hand.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    fault.execute_batch("DROP TRIGGER no_update;").unwrap();
    let changes = until_changes(&daemon, 1).await;
    assert_eq!(
        changes
            .iter()
            .map(|change| (change.from.memory, change.to.memory, change.uncertain))
            .collect::<Vec<_>>(),
        [(7 << 30, 5376 << 20, true)],
        "listed as uncertain, with what it replaced"
    );
    assert!(
        daemon
            .store
            .auto_sizing()
            .await
            .unwrap()
            .last
            .contains_key("node"),
        "it holds node's interval"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), by_hand);
    daemon.stop().await;
}

#[tokio::test]
async fn a_change_left_pending_is_settled_with_auto_sizing_turned_off() {
    let dir = Dir::new();
    // Written before a crash; the owner then turned auto sizing off.
    let written = EXAMPLE.replacen("memory = \"7G\"", "memory = \"5.25G\"", 1);
    let daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &written).await;
    let change = SizingChange {
        at: super::history::now(),
        profile: "node".to_owned(),
        from: Resize {
            memory: 7 << 30,
            slots: 2,
            cpus: None,
        },
        to: Resize {
            memory: 5376 << 20,
            slots: 2,
            cpus: None,
        },
        reason: "lower memory".to_owned(),
        uncertain: false,
    };
    daemon.store.sizing_intent(change.clone()).await.unwrap();
    assert_eq!(until_changes(&daemon, 1).await, vec![change]);
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), written);
    daemon.stop().await;
}

#[tokio::test]
async fn a_save_made_from_a_revision_is_refused_once_the_file_was_edited_on_disk() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let response = daemon
        .client
        .get("http://runnerd/v1/config")
        .send()
        .await
        .unwrap();
    let read: ConfigFile = serde_json::from_slice(&body(response).await).unwrap();
    let edited = format!("{EXAMPLE}\n# Edited by hand.\n");
    std::fs::write(&daemon.config, &edited).unwrap();
    let response = daemon
        .client
        .put(format!(
            "http://runnerd/v1/config?revision={}",
            read.revision
        ))
        .body(EXAMPLE.replacen("slots = 2", "slots = 1", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), edited);
    daemon.stop().await;
}

#[tokio::test]
async fn a_config_made_from_a_revision_replaced_since_is_refused() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let read = |daemon: &Daemon| {
        let client = daemon.client.clone();
        async move {
            let response = client.get("http://runnerd/v1/config").send().await.unwrap();
            serde_json::from_slice::<ConfigFile>(&body(response).await).unwrap()
        }
    };
    let first = read(&daemon).await;
    let first_revision = first.revision.clone();
    let put = |text: String, revision: &str| {
        daemon
            .client
            .put(format!("http://runnerd/v1/config?revision={revision}"))
            .body(text)
            .send()
    };
    // As auto sizing would, between the editor's read and its save.
    let changed = EXAMPLE.replacen("slots = 2", "slots = 1", 1);
    let response = put(changed.clone(), &first_revision).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let saved: ConfigCheck = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(
        saved.revision,
        Some(runnerd_core::config::revision(&changed)),
        "the reply has the revision of the text written"
    );
    let stale = EXAMPLE.replacen("memory = \"7G\"", "memory = \"6G\"", 1);
    let response = put(stale, &first_revision).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), changed);
    let second = read(&daemon).await;
    assert_ne!(second.revision, first.revision);
    daemon.stop().await;
}

#[tokio::test]
async fn auto_sizing_off_leaves_the_file_as_written() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    finished_jobs(&daemon.store, "node", 7 << 30, 2 << 30).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_to_string(&daemon.config).unwrap(), EXAMPLE);
    assert_eq!(
        read_sizing(&daemon).await.auto_plan,
        None,
        "nothing is planned"
    );
    daemon.stop().await;
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
async fn signing_every_browser_out_replaces_the_dashboards_session_secret() {
    let (client, _control, dir) = daemon().await;
    let file = dir.0.join("dashboard-session");
    let before = std::fs::read_to_string(&file).unwrap();
    let response = client
        .post("http://runnerd/v1/dashboard/sign-out")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(before != after, "the old secret no longer signs in");
}

/// Opens `link` on the dashboard at `addr` as a browser at `origin` does through a proxy, and returns the status.
async fn open_link(addr: std::net::SocketAddr, link: &str, origin: &str) -> StatusCode {
    let path = &link[link.find("/login?").unwrap()..];
    let host = origin.split_once("://").unwrap().1;
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("http://{addr}{path}"))
        .header("host", host)
        .header("x-forwarded-proto", origin.split_once("://").unwrap().0)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn pairing_links_from_the_socket_are_fresh_and_sign_one_device_in_on_their_origin_only() {
    const PROXY: &str = "https://forge.example.ts.net:8443";
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let pair = |query: &'static str| {
        daemon
            .client
            .post(format!("http://runnerd/v1/dashboard/pairings{query}"))
            .send()
    };
    let off = pair("").await.unwrap();
    assert_eq!(off.status(), StatusCode::CONFLICT, "no dashboard runs yet");
    let addr = dashboard_for(&dir.0, &daemon, &[PROXY.to_owned()]).await;
    let mut links = Vec::new();
    for _ in 0..2 {
        let reply = pair("").await.unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        let pairings: Pairings = serde_json::from_slice(&body(reply).await).unwrap();
        assert_eq!(pairings.links.len(), 1, "one per origin");
        assert_eq!(pairings.links[0].origin, PROXY);
        assert_eq!(pairings.links[0].lifetime, 300);
        links.push(pairings.links[0].url.clone());
    }
    assert_ne!(links[0], links[1], "each request makes a new code");
    for elsewhere in [&format!("http://{addr}"), "https://forge.lan"] {
        assert_eq!(
            open_link(addr, &links[0], elsewhere).await,
            StatusCode::UNAUTHORIZED,
            "not on {elsewhere}"
        );
    }
    assert_eq!(
        open_link(addr, &links[0], PROXY).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        open_link(addr, &links[0], PROXY).await,
        StatusCode::UNAUTHORIZED,
        "once only"
    );
    daemon
        .client
        .post("http://runnerd/v1/dashboard/sign-out")
        .send()
        .await
        .unwrap();
    assert_eq!(
        open_link(addr, &links[1], PROXY).await,
        StatusCode::UNAUTHORIZED,
        "a link made before signing every device out no longer works"
    );
    let unlisted = pair("?origin=https%3A%2F%2Fother.example").await.unwrap();
    assert_eq!(unlisted.status(), StatusCode::BAD_REQUEST);
}

/// A dashboard sharing `daemon`'s credentials, taking writes from `proxies`; its address.
async fn dashboard_for(dir: &Path, daemon: &Daemon, proxies: &[String]) -> std::net::SocketAddr {
    let listener = runnerd_web::listen("127.0.0.1:0").await.unwrap().unwrap();
    let addr = listener.local_addr().unwrap();
    runnerd_web::spawn(
        listener,
        dir.join("runnerd.sock"),
        proxies
            .iter()
            .map(|origin| origin.parse().unwrap())
            .collect(),
        daemon.sessions.clone(),
        std::future::pending::<()>(),
    );
    for _ in 0..100 {
        if runnerd_web::login_file(&dir.join("runnerd.sock")).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    addr
}

#[tokio::test]
async fn as_many_origins_as_live_codes_each_get_a_link_that_works_and_more_get_none() {
    let origins = |count: usize| -> Vec<String> {
        (1..=count)
            .map(|n| format!("https://forge-{n}.example.ts.net:8443"))
            .collect()
    };
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let addr = dashboard_for(&dir.0, &daemon, &origins(8)).await;
    let reply = daemon
        .client
        .post("http://runnerd/v1/dashboard/pairings")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    let pairings: Pairings = serde_json::from_slice(&body(reply).await).unwrap();
    assert_eq!(pairings.links.len(), 8);
    for link in &pairings.links {
        assert_eq!(
            open_link(addr, &link.url, &link.origin).await,
            StatusCode::SEE_OTHER,
            "the link on {} works",
            link.origin
        );
    }

    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let addr = dashboard_for(&dir.0, &daemon, &origins(9)).await;
    let one = daemon
        .client
        .post("http://runnerd/v1/dashboard/pairings?origin=https%3A%2F%2Fforge-1.example.ts.net%3A8443")
        .send()
        .await
        .unwrap();
    let one: Pairings = serde_json::from_slice(&body(one).await).unwrap();
    let reply = daemon
        .client
        .post("http://runnerd/v1/dashboard/pairings")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::CONFLICT);
    let message = String::from_utf8(body(reply).await).unwrap();
    assert!(message.contains("--base"), "{message}");
    assert_eq!(
        open_link(addr, &one.links[0].url, &one.links[0].origin).await,
        StatusCode::SEE_OTHER,
        "the refused batch made no code that pushed out one made before"
    );
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
        client.post("http://runnerd/v1/dashboard/sign-out"),
        client.post("http://runnerd/v1/dashboard/pairings"),
        client.get("http://runnerd/v1/jobs"),
        client.get("http://runnerd/v1/jobs/1"),
        client.get("http://runnerd/v1/jobs/1/samples"),
        client.get("http://runnerd/v1/jobs/1/logs"),
        client.get("http://runnerd/v1/host/samples"),
        client.get("http://runnerd/v1/stats"),
        client.get("http://runnerd/v1/activity"),
        client.get("http://runnerd/v1/dashboard"),
        client.get("http://runnerd/v1/sizing"),
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
    let daemon = start_with(&dir.0, None, false, sync_dir, memory_max).await;
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
    let mut daemon = start_with(&dir.0, None, true, failing_sync, memory_max).await;
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
async fn a_valid_config_replaces_the_file_it_links_to_readable_by_its_user_only_and_is_reloaded() {
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
    // It was 0640: a saved config may hold notification secrets.
    let mode = std::fs::metadata(&daemon.config).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
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
async fn the_config_api_never_shows_a_notify_secret_and_saving_what_it_shows_keeps_them() {
    let dir = Dir::new();
    let mut daemon = start(&dir.0, None).await;
    let secrets = format!(
        "{EXAMPLE}\n[[notify.targets]]\nname = \"phone\"\nkind = \"ntfy\"\n\
         url = \"http://127.0.0.1:9/s3cret-topic\" # the topic\n\
         headers = {{ Authorization = \"Bearer tk_hunter2\" }}\n"
    );
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(secrets.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    daemon.reloads.recv().await.unwrap();

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
    for secret in ["s3cret", "hunter2"] {
        assert!(!shown.text.contains(secret), "{}", shown.text);
    }
    assert!(
        shown
            .text
            .contains("url = \"<secret>\" # the topic\nheaders = { Authorization = \"<secret>\" }"),
        "{}",
        shown.text
    );

    // What a browser checks and saves: the text it was shown, with a change.
    let edited = shown.text.replacen("slots = 2", "slots = 1", 1);
    let checked: ConfigCheck = serde_json::from_slice(
        &body(
            daemon
                .client
                .post("http://runnerd/v1/config/check")
                .body(edited.clone())
                .send()
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert!(checked.valid, "{}", checked.message);
    let response = daemon
        .client
        .put(format!(
            "http://runnerd/v1/config?revision={}",
            shown.revision
        ))
        .body(edited)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read_to_string(&daemon.config).unwrap(),
        secrets.replacen("slots = 2", "slots = 1", 1),
        "the file keeps its secrets"
    );

    // A target renamed in the editor has no secret to keep: refused, without the value.
    let renamed = shown.text.replace("name = \"phone\"", "name = \"desk\"");
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(renamed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let refused: ConfigCheck = serde_json::from_slice(&body(response).await).unwrap();
    assert!(
        refused
            .message
            .contains("notify target \"desk\": url is <secret>"),
        "{}",
        refused.message
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_broken_edit_of_the_masked_config_answers_an_error_without_the_secrets() {
    let dir = Dir::new();
    let mut daemon = start(&dir.0, None).await;
    let secrets = format!(
        "{EXAMPLE}\n[notify]\ntargets = [{{ name = \"phone\", kind = \"ntfy\", \
         url = \"http://127.0.0.1:9/s3cret-topic\", headers = {{ Authorization = \"Bearer tk_hunter2\" }} }}]\n"
    );
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(secrets.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    daemon.reloads.recv().await.unwrap();
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

    // A typo on the line that holds the masks, and a header that is not a string beside one.
    for broken in [
        shown.text.replace("kind = \"ntfy\"", "kind = \"ntfyy\""),
        shown.text.replace(
            "Authorization = \"<secret>\"",
            "Authorization = \"<secret>\", X = 5",
        ),
    ] {
        assert_ne!(broken, shown.text);
        let checked = daemon
            .client
            .post("http://runnerd/v1/config/check")
            .body(broken.clone())
            .send()
            .await
            .unwrap();
        let saved = daemon
            .client
            .put("http://runnerd/v1/config")
            .body(broken)
            .send()
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::UNPROCESSABLE_ENTITY);
        for answer in [body(checked).await, body(saved).await] {
            let answer: ConfigCheck = serde_json::from_slice(&answer).unwrap();
            assert!(!answer.valid, "refused");
            assert!(answer.message.contains("<secret>"), "{}", answer.message);
            for secret in ["s3cret", "hunter2"] {
                assert!(!answer.message.contains(secret), "{}", answer.message);
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(&daemon.config).unwrap(),
        secrets,
        "the file as it was"
    );
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
            admission: runnerd_core::api::AdmissionPolicy::Caps,
            held: false,
            slice: "ci.slice".to_owned(),
            memory_max: None,
            memory_in_use,
            mirror_memory: 0,
            seed_memory: Vec::new(),
            shell_memory: Vec::new(),
            slice_cpus: None,
            host: None,
            cpu: None,
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
async fn the_slices_samples_go_back_a_day_at_most() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for at in [now - 2 * 24 * 3600, now - 3600] {
        daemon.store.host_sample(HostSample {
            at,
            cpu: 1,
            memory: 1,
        });
    }
    daemon.store.flush().await;
    let response = daemon
        .client
        .get("http://runnerd/v1/host/samples?since=0")
        .send()
        .await
        .unwrap();
    let samples: HostSamples = serde_json::from_slice(&body(response).await).unwrap();
    let times: Vec<u64> = samples.samples.iter().map(|sample| sample.at).collect();
    assert_eq!(times, [now - 3600]);
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
        admission: None,
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

#[tokio::test]
async fn records_are_removed_one_by_one_or_by_a_selection_the_request_names() {
    let (client, _, _dir) = daemon().await;
    let delete = async |path: &str| {
        client
            .delete(format!("http://runnerd{path}"))
            .send()
            .await
            .unwrap()
    };
    for unnamed in [
        "/v1/records",
        "/v1/records?older_than=soon",
        "/v1/records?all=true&older_than=2d",
    ] {
        assert_eq!(
            delete(unnamed).await.status(),
            StatusCode::BAD_REQUEST,
            "{unnamed}"
        );
    }
    let removed = delete("/v1/records?older_than=2d").await;
    assert_eq!(removed.status(), StatusCode::OK);
    let removed: RemovedRecords = serde_json::from_slice(&body(removed).await).unwrap();
    assert_eq!(removed.jobs, [8]);
    assert_eq!(delete("/v1/records/7").await.status(), StatusCode::CONFLICT);
    let listed = client
        .get("http://runnerd/v1/records")
        .send()
        .await
        .unwrap();
    let listed: Records = serde_json::from_slice(&body(listed).await).unwrap();
    assert_eq!(listed, Records::default());
}

/// Job `id` of o/app, failed at its step 2, in the history.
fn failed_job(store: &Store, id: u64) {
    let step = |number: u32, name: &str, conclusion: &str| runnerd_core::github::JobStep {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: None,
        completed_at: None,
    };
    store.queued(
        "o/app",
        &QueuedJob {
            id,
            run_id: 2,
            name: "build".to_owned(),
            ..QueuedJob::default()
        },
    );
    let runner = format!("runner-{id}");
    store.runner_started(RunnerRecord {
        admission: None,
        name: runner.clone(),
        repository: "o/app".to_owned(),
        profile: "app".to_owned(),
        slot: 1,
        container: "c1".to_owned(),
        started_at: 1_000,
        memory_cap: Some(4 << 30),
        cpu_cap: Some(2000),
    });
    store.claimed(id, 2, &runner, 2_000);
    store.details(JobDetails {
        id,
        run_id: 2,
        run_attempt: 1,
        name: "build".to_owned(),
        workflow_name: Some("ci".to_owned()),
        head_branch: None,
        html_url: None,
        labels: Vec::new(),
        created_at: None,
        status: "completed".to_owned(),
        conclusion: Some("failure".to_owned()),
        started_at: None,
        completed_at: None,
        runner_name: Some(runner),
        steps: vec![
            step(1, "Set up job", "success"),
            step(2, "Run make", "failure"),
        ],
    });
}

/// Sends `POST /v1/records/{job}/shell` on its own connection, upgrading it unless `upgrade` is false, and returns
/// the connection with what the daemon answered so far.
async fn open_shell(socket: &Path, job: u64, upgrade: bool) -> (tokio::net::UnixStream, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let headers = if upgrade {
        "Connection: Upgrade\r\nUpgrade: runnerd-tty\r\n"
    } else {
        ""
    };
    let request = format!(
        "POST /v1/records/{job}/shell?cols=100&rows=30 HTTP/1.1\r\nHost: runnerd\r\n{headers}Content-Length: 0\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let mut buffer = [0; 4096];
    while !String::from_utf8_lossy(&answer).contains("\r\n\r\n") {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(
            read > 0,
            "the daemon closed: {}",
            String::from_utf8_lossy(&answer)
        );
        answer.extend_from_slice(&buffer[..read]);
    }
    (stream, String::from_utf8_lossy(&answer).into_owned())
}

async fn closed_shell(daemon: &mut Daemon) -> String {
    tokio::time::timeout(Duration::from_secs(5), daemon.closed.recv())
        .await
        .expect("the shell was never closed")
        .unwrap()
}

#[tokio::test]
async fn a_shell_runs_over_the_socket_and_is_removed_once_it_exits() {
    use tokio::io::AsyncReadExt;
    let dir = Dir::new();
    let mut daemon = start(&dir.0, None).await;
    failed_job(&daemon.store, 9);
    daemon.store.flush().await;
    let socket = dir.0.join("runnerd.sock");

    let (_, refused) = open_shell(&socket, 9, false).await;
    assert!(refused.starts_with("HTTP/1.1 426"), "{refused}");
    let (_, refused) = open_shell(&socket, 11, true).await;
    assert!(
        refused.starts_with("HTTP/1.1 404"),
        "a job the history lacks: {refused}"
    );

    let (mut stream, answer) = open_shell(&socket, 9, true).await;
    assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
    let mut terminal = answer.split_once("\r\n\r\n").unwrap().1.to_owned();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut rest = String::new();
        stream.read_to_string(&mut rest).await.unwrap();
        terminal.push_str(&rest);
    })
    .await
    .expect("the terminal never ended with its shell");
    assert!(
        terminal.contains("It failed at step 2: Run make"),
        "{terminal}"
    );
    assert!(
        terminal.ends_with("$ "),
        "the shell's output follows the banner: {terminal}"
    );
    assert_eq!(closed_shell(&mut daemon).await, "shell-9");
}

#[tokio::test]
async fn a_shell_whose_client_goes_away_is_removed() {
    let dir = Dir::new();
    let mut daemon = start(&dir.0, None).await;
    failed_job(&daemon.store, 10);
    failed_job(&daemon.store, 12);
    daemon.store.flush().await;
    let socket = dir.0.join("runnerd.sock");

    let (_, refused) = open_shell(&socket, 12, true).await;
    assert!(refused.starts_with("HTTP/1.1 409"), "{refused}");
    assert!(refused.contains("the budget is full"), "{refused}");

    let (stream, answer) = open_shell(&socket, 10, true).await;
    assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
    drop(stream);
    assert_eq!(closed_shell(&mut daemon).await, "shell-10");
}

#[tokio::test]
async fn a_cpu_budget_needs_the_slices_cpus_and_a_config_without_one_never_reads_them() {
    let dir = Dir::new();
    // This daemon's slice cannot report its CPUs (`unreadable_cpus`).
    let mut daemon = start(&dir.0, None).await;
    let before = std::fs::read(&daemon.config).unwrap();
    let with_budget = EXAMPLE.replacen(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
        1,
    );
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(with_budget)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let refused: ConfigCheck = serde_json::from_slice(&body(response).await).unwrap();
    assert!(
        !refused.valid && refused.message.contains("cannot be read"),
        "{}",
        refused.message
    );
    assert_eq!(std::fs::read(&daemon.config).unwrap(), before);
    assert!(daemon.reloads.try_recv().is_err());

    let without = EXAMPLE.replacen("slots = 2", "slots = 1", 1);
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(without)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        daemon.reloads.recv().await.unwrap().host.cpu_overcommit,
        None
    );
}

#[tokio::test]
async fn a_notify_test_says_what_each_target_did_without_its_url() {
    let dir = Dir::new();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap().to_string();
    drop(closed);
    let text = format!(
        "{EXAMPLE}\n[[notify.targets]]\nname = \"phone\"\nkind = \"ntfy\"\nurl = \"http://{address}/s3cret-topic\"\n"
    );
    let daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &text).await;

    let response = daemon
        .client
        .post("http://runnerd/v1/notify/test")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let tested: runnerd_core::api::NotifyTest =
        serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(tested.targets.len(), 1);
    assert_eq!(tested.targets[0].target, "phone");
    let error = tested.targets[0]
        .error
        .clone()
        .expect("nothing listens there");
    assert_eq!(error, "cannot connect to the target");

    let response = daemon
        .client
        .post("http://runnerd/v1/notify/test?target=desk")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let refused: ErrorResponse = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(refused.message, "[notify] has no target named \"desk\"");
}

#[tokio::test]
async fn a_notify_test_asks_every_target_at_once_so_slow_ones_fit_the_clients_wait() {
    let dir = Dir::new();
    // Each target answers only once both were asked: one after the other, the first never answers.
    let both = Arc::new(tokio::sync::Barrier::new(2));
    let app = axum::Router::new().fallback(axum::routing::post(move || {
        let both = both.clone();
        async move {
            both.wait().await;
            StatusCode::OK
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let text = format!(
        "{EXAMPLE}\n[[notify.targets]]\nname = \"phone\"\nkind = \"webhook\"\nurl = \"http://{address}/phone\"\n\
         [[notify.targets]]\nname = \"desk\"\nkind = \"webhook\"\nurl = \"http://{address}/desk\"\n"
    );
    let daemon = start_serving(&dir.0, None, true, sync_dir, memory_max, &text).await;

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        daemon.client.post("http://runnerd/v1/notify/test").send(),
    )
    .await
    .expect("the test answers within one target's timeout")
    .unwrap();
    let tested: runnerd_core::api::NotifyTest =
        serde_json::from_slice(&body(response).await).unwrap();
    let delivered: Vec<(&str, Option<&str>)> = tested
        .targets
        .iter()
        .map(|delivery| (delivery.target.as_str(), delivery.error.as_deref()))
        .collect();
    assert_eq!(delivered, [("phone", None), ("desk", None)]);
}

#[tokio::test]
async fn insights_and_the_digest_read_the_history() {
    let (client, _, _dir) = daemon().await;

    let response = client
        .get("http://runnerd/v1/insights?days=7")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let insights: runnerd_core::api::Insights =
        serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(insights.until - insights.since, 7 * 24 * 3600);
    assert!(
        insights.insights.is_empty(),
        "an empty history says nothing"
    );

    let response = client.get("http://runnerd/v1/digest").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let digest: runnerd_core::api::Digest = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!((digest.finished, digest.failed), (0, 0));
}

#[tokio::test]
async fn the_savings_leave_lost_jobs_out_and_say_so() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    // Lost in the second the request is made, after the history began with the database.
    let lost_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        * 1000
        + 20_000;
    finished_jobs_at(&daemon.store, "app", 4 << 30, 1 << 30, lost_now).await;
    daemon.store.visibility(
        "o/app".to_owned(),
        runnerd_core::api::Visibility::Private,
        0,
    );
    daemon.store.flush().await;
    let response = daemon
        .client
        .get("http://runnerd/v1/savings")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let savings: runnerd_core::api::Savings =
        serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!((savings.month.jobs, savings.month.lost), (0, 10));
    assert!(
        savings.month.headline().ends_with("(10 jobs not priced)"),
        "{}",
        savings.month.headline()
    );
    daemon.stop().await;
}
