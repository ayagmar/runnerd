//! The notifier against a fake HTTP endpoint on loopback: a failure delivered once across a restart, a burst folded
//! into one message, a failing endpoint retried while the history goes on, a target added later replaying nothing,
//! an insight told once, the daily digest, ntfy's JSON, and no URL or token in a log line.

use std::io::Write as _;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::post;
use runnerd_core::api::{
    Event, Insight, InsightKind, JobMetrics, JobState, JobSummary, LogSummary, Notification,
    NotifyEvent, Priority, Severity, Timeline,
};
use runnerd_core::config::{Config, DayTime};
use runnerd_core::store::Store;
use tokio::sync::{Mutex, watch};

use super::{
    ConfigText, Notifier, Setup, Timing, client, digest_due, parse_offset, run, send_due, start,
};
use crate::testdir::Dir;

const SETUP: Setup = Setup {
    timing: Timing {
        gather: Duration::from_millis(100),
        gap: Duration::from_millis(100),
        retry: Duration::from_millis(50),
        retry_max: Duration::from_millis(100),
        check: Duration::from_secs(60),
        timeout: Duration::from_secs(2),
    },
    offset: || Some(0),
};

const CONFIG: &str = r#"
[host]
slice = "runnerd.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.app]
image_stage = "node"
cpus = 4
memory = "4G"
slots = 2

[[repositories]]
name = "halyard/api"
profile = "app"
labels = ["app"]
"#;

/// What the fake endpoint received: path and query, headers, body, and the status it answered.
type Received = Arc<SyncMutex<Vec<(String, HeaderMap, Vec<u8>, u16)>>>;

/// A loopback endpoint that records every post and answers `status`.
struct Endpoint {
    address: String,
    received: Received,
    status: Arc<AtomicU16>,
}

impl Endpoint {
    async fn start() -> Endpoint {
        let received: Received = Arc::default();
        let status = Arc::new(AtomicU16::new(200));
        let (kept, answer) = (received.clone(), status.clone());
        let app = Router::new().fallback(post(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let (kept, answer) = (kept.clone(), answer.clone());
            async move {
                let status = answer.load(Ordering::SeqCst);
                kept.lock()
                    .unwrap()
                    .push((uri.to_string(), headers, body.to_vec(), status));
                StatusCode::from_u16(status).unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Endpoint {
            address,
            received,
            status,
        }
    }

    fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }

    /// The webhook notifications received, in order.
    fn notifications(&self) -> Vec<Notification> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, body, _)| serde_json::from_slice(body).unwrap())
            .collect()
    }

    /// Waits until it received `count` posts, then a little longer for any it should not get.
    async fn wait_for(&self, count: usize) {
        for _ in 0..200 {
            if self.count() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// `CONFIG` with a webhook target `phone` at `url`.
fn webhook(url: &str) -> String {
    format!(
        "{CONFIG}\n[[notify.targets]]\nname = \"phone\"\nkind = \"webhook\"\nurl = \"{url}\"\n\
         headers = {{ Authorization = \"Bearer tk_hunter2\" }}\n"
    )
}

/// A finished job of `halyard/api`.
fn finished(id: u64, state: JobState, conclusion: Option<&str>) -> Event {
    Event::JobFinished {
        job: JobSummary {
            id,
            repository: "halyard/api".to_owned(),
            run_id: 7,
            run_attempt: 1,
            workflow: Some("CI".to_owned()),
            name: format!("test {id}"),
            branch: Some("main".to_owned()),
            url: Some(format!(
                "https://github.com/halyard/api/actions/runs/7/job/{id}"
            )),
            labels: vec!["app".to_owned()],
            state,
            conclusion: conclusion.map(str::to_owned),
            runner: None,
            timeline: Timeline::default(),
            metrics: JobMetrics::default(),
            log: LogSummary::default(),
        },
    }
}

fn failed(id: u64) -> Event {
    finished(id, JobState::Completed, Some("failure"))
}

/// A running notifier over `store`, reading the config from `config`.
struct Running {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn start(store: &Store, config: &Arc<Mutex<ConfigText>>) -> Running {
        let (stop, stopping) = watch::channel(false);
        let task = tokio::spawn(run(Notifier {
            store: store.clone(),
            config: config.clone(),
            stopping,
            setup: SETUP,
        }));
        // Lets it read where each target got to before the test stores events.
        tokio::time::sleep(Duration::from_millis(100)).await;
        Running { stop, task }
    }

    async fn stop(self) {
        self.stop.send(true).unwrap();
        self.task.await.unwrap();
    }
}

fn config_text(dir: &Dir, text: &str) -> Arc<Mutex<ConfigText>> {
    Arc::new(Mutex::new(ConfigText {
        path: dir.0.join("runnerd.toml"),
        text: text.to_owned(),
    }))
}

#[tokio::test]
async fn a_failure_is_delivered_once_across_a_restart() {
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let config = config_text(&dir, &webhook(&format!("http://{}/hook", endpoint.address)));

    let notifier = Running::start(&store, &config).await;
    store.event(finished(1, JobState::Completed, Some("success")));
    store.event(failed(2));
    endpoint.wait_for(1).await;
    notifier.stop().await;
    // Fails while the daemon is down.
    store.event(failed(3));
    store.flush().await;

    let notifier = Running::start(&store, &config).await;
    endpoint.wait_for(2).await;
    notifier.stop().await;

    let jobs: Vec<Vec<u64>> = endpoint
        .notifications()
        .iter()
        .map(|sent| sent.jobs.iter().map(|job| job.id).collect())
        .collect();
    assert_eq!(
        jobs,
        [vec![2], vec![3]],
        "job 2 once, and job 3, which failed while it was down, after the restart"
    );
    let first = &endpoint.notifications()[0];
    assert_eq!(first.event, NotifyEvent::Failed);
    assert_eq!(first.title, "halyard/api: CI / test 2 failed");
    assert_eq!(first.priority, Priority::High);
    let headers = &endpoint.received.lock().unwrap()[0].1;
    assert_eq!(headers["authorization"], "Bearer tk_hunter2");
}

#[tokio::test]
async fn a_burst_of_failures_is_one_message() {
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let config = config_text(&dir, &webhook(&format!("http://{}/hook", endpoint.address)));
    let notifier = Running::start(&store, &config).await;

    for id in 1..=30 {
        store.event(failed(id));
    }
    store.event(finished(31, JobState::Lost, None));
    endpoint.wait_for(1).await;
    notifier.stop().await;

    let sent = endpoint.notifications();
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert_eq!(sent[0].title, "30 jobs failed, 1 job lost");
    assert_eq!(
        sent[0].jobs.len(),
        21,
        "the first 20 failures and the lost job: a red main does not grow the body without end"
    );
    let lines: Vec<&str> = sent[0].message.lines().collect();
    assert_eq!(
        lines.len(),
        7,
        "five failures, how many more, the lost one: {lines:#?}"
    );
    assert_eq!(lines[5], "and 25 more");
}

/// A warning that `halyard/api`'s test is flaky.
fn flaky() -> Insight {
    Insight {
        key: "flaky:halyard/api/CI/test".to_owned(),
        severity: Severity::Warning,
        repository: "halyard/api".to_owned(),
        workflow: Some("CI".to_owned()),
        job: Some("test".to_owned()),
        sentence: "halyard/api: CI / test failed and then passed on a re-run in 2 of its 9 runs."
            .to_owned(),
        kind: InsightKind::Flaky { flaky: 2, total: 9 },
        runs: Vec::new(),
    }
}

#[tokio::test]
async fn an_insight_is_told_once_and_again_only_after_it_went_away_for_a_day() {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    let timing = Timing {
        gather: Duration::ZERO,
        gap: Duration::ZERO,
        ..SETUP.timing
    };
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let config = Config::parse(&webhook(&format!("http://{}/hook", endpoint.address))).unwrap();
    let phone = &config.notify.targets[0];
    let http = client(timing.timeout).unwrap();
    let mut target = start(&store, None, phone, &config).await.unwrap();
    let t0 = 1_800_000_000_000;
    let looks = async |target: &mut super::Target, held: Vec<Insight>, at: u64| {
        target.look(&held, at);
        send_due(&store, &http, &timing, target).await;
    };

    looks(&mut target, vec![flaky()], t0).await;
    for minutes in [5, 10, 15] {
        looks(&mut target, vec![flaky()], t0 + minutes * MINUTE).await;
    }
    assert_eq!(endpoint.count(), 1, "held across several looks: told once");

    // A restart remembers it was told.
    let kept = store.notify_state().await.unwrap().remove("phone");
    let mut target = start(&store, kept, phone, &config).await.unwrap();
    looks(&mut target, vec![flaky()], t0 + 20 * MINUTE).await;
    // Gone for less than a day and back: not again.
    looks(&mut target, Vec::new(), t0 + HOUR).await;
    looks(&mut target, vec![flaky()], t0 + 12 * HOUR).await;
    // Held for more than a day since it was told: still not again.
    looks(&mut target, vec![flaky()], t0 + 30 * HOUR).await;
    assert_eq!(
        endpoint.count(),
        1,
        "told once across a restart, a short gap and a long hold"
    );

    // Gone for more than a day and back: told again.
    looks(&mut target, Vec::new(), t0 + 31 * HOUR).await;
    looks(&mut target, Vec::new(), t0 + 56 * HOUR).await;
    looks(&mut target, vec![flaky()], t0 + 57 * HOUR).await;
    let sent = endpoint.notifications();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[1].event, NotifyEvent::Insights);
    assert_eq!(sent[1].title, "halyard/api: CI / test is flaky");
}

#[tokio::test]
async fn a_digest_delivered_after_midnight_counts_for_the_day_it_was_made() {
    const HOUR: u64 = 3600;
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let text = webhook(&format!("http://{}/hook", endpoint.address));
    let config = Config::parse(&text).unwrap();
    let http = client(SETUP.timing.timeout).unwrap();
    let mut target = start(&store, None, &config.notify.targets[0], &config)
        .await
        .unwrap();
    let today = super::now();
    let midnight = today - today % (24 * HOUR);
    let late = DayTime {
        minutes: 23 * 60 + 50,
    };
    // Made at 23:50 UTC yesterday; the endpoint answers only today.
    target.digest = Some(runnerd_core::insights::digest(
        &[],
        &config,
        (midnight - 10 * 60) * 1000,
    ));
    send_due(&store, &http, &SETUP.timing, &mut target).await;

    assert_eq!(endpoint.count(), 1);
    let last = store.notify_state().await.unwrap()["phone"]["digest"] / 1000;
    assert!(
        digest_due(midnight + 23 * HOUR + 50 * 60, 0, late, last),
        "today's digest still goes at 23:50"
    );
}

#[tokio::test]
async fn the_digest_goes_once_at_its_time_across_a_restart() {
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    // It last went long ago, and is due from midnight.
    assert!(
        store
            .notify_record(
                "phone",
                vec![("event".to_owned(), 0), ("digest".to_owned(), 0)],
                Vec::new()
            )
            .await,
        "recorded"
    );
    let text = format!(
        "{CONFIG}\n[notify]\ndigest_at = \"00:00\"\n\n[[notify.targets]]\nname = \"phone\"\n\
         kind = \"webhook\"\nurl = \"http://{}/hook\"\nevents = [\"digest\"]\n",
        endpoint.address
    );
    let config = config_text(&dir, &text);
    let notifier = Running::start(&store, &config).await;
    store.event(failed(1));
    endpoint.wait_for(1).await;
    notifier.stop().await;
    let notifier = Running::start(&store, &config).await;
    endpoint.wait_for(2).await;
    notifier.stop().await;

    let sent = endpoint.notifications();
    assert_eq!(
        sent.len(),
        1,
        "once a day, not again after a restart: {sent:#?}"
    );
    assert_eq!(sent[0].event, NotifyEvent::Digest);
    assert_eq!(sent[0].title, "CI in the last 24 hours");
    assert_eq!(sent[0].priority, Priority::Low);
    assert!(sent[0].jobs.is_empty(), "failed is not among its events");
    assert!(
        sent[0].digest.is_some(),
        "the digest's figures come with it"
    );
}

#[tokio::test]
async fn a_failing_endpoint_is_retried_without_holding_back_the_history() {
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    endpoint.status.store(502, Ordering::SeqCst);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let config = config_text(&dir, &webhook(&format!("http://{}/hook", endpoint.address)));
    let notifier = Running::start(&store, &config).await;

    store.event(failed(1));
    endpoint.wait_for(2).await;
    // The history takes writes as before while the target refuses.
    store.event(failed(2));
    tokio::time::timeout(Duration::from_secs(1), store.flush())
        .await
        .expect("the store is not held back");
    endpoint.status.store(200, Ordering::SeqCst);
    endpoint.wait_for(endpoint.count() + 1).await;
    notifier.stop().await;

    let received = endpoint.received.lock().unwrap().clone();
    let refused = received.iter().filter(|post| post.3 == 502).count();
    assert!(refused >= 2, "retried: {refused} posts refused");
    let delivered: Vec<Notification> = received
        .iter()
        .filter(|post| post.3 == 200)
        .map(|post| serde_json::from_slice(&post.2).unwrap())
        .collect();
    assert_eq!(delivered.len(), 1, "delivered once, then nothing");
    assert_eq!(
        delivered[0]
            .jobs
            .iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        [1, 2],
        "the job that came while it was refused is folded in"
    );
}

#[tokio::test]
async fn nothing_is_sent_without_notify_and_a_target_added_later_replays_nothing() {
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let config = config_text(&dir, CONFIG);
    let notifier = Running::start(&store, &config).await;

    store.event(failed(1));
    store.flush().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        store.notify_state().await.unwrap().is_empty(),
        "without [notify] the notifier keeps nothing"
    );

    config.lock().await.text = webhook(&format!("http://{}/hook", endpoint.address));
    // Any durable event wakes it, and it reads the config again.
    store.event(finished(2, JobState::Completed, Some("success")));
    tokio::time::sleep(Duration::from_millis(300)).await;
    store.event(failed(3));
    endpoint.wait_for(1).await;
    notifier.stop().await;

    let jobs: Vec<Vec<u64>> = endpoint
        .notifications()
        .iter()
        .map(|sent| sent.jobs.iter().map(|job| job.id).collect())
        .collect();
    assert_eq!(jobs, [vec![3]], "job 1 failed before the target existed");
}

#[tokio::test]
async fn an_ntfy_target_gets_ntfys_json_at_the_server_with_its_topic() {
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let text = format!(
        "{CONFIG}\n[[notify.targets]]\nname = \"phone\"\nkind = \"ntfy\"\n\
         url = \"http://{}/halyard-ci-6f3a\"\nevents = [\"lost\"]\n",
        endpoint.address
    );
    let config = config_text(&dir, &text);
    let notifier = Running::start(&store, &config).await;

    store.event(failed(1));
    store.event(finished(2, JobState::Lost, None));
    endpoint.wait_for(1).await;
    notifier.stop().await;

    let received = endpoint.received.lock().unwrap().clone();
    assert_eq!(
        received.len(),
        1,
        "only the lost job: failed is not among its events"
    );
    let (path, _, body, _) = &received[0];
    assert_eq!(path, "/");
    let body: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(body["topic"], "halyard-ci-6f3a");
    assert_eq!(body["title"], "halyard/api: CI / test 2 lost its runner");
    assert_eq!(body["priority"], 3);
    assert_eq!(
        body["click"],
        "https://github.com/halyard/api/actions/runs/7/job/2"
    );
}

/// Log lines written while it is set as the thread's subscriber.
#[derive(Clone, Default)]
struct Captured(Arc<SyncMutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn no_log_line_shows_a_targets_url_or_token() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    // With one subscriber registered, tracing asks only the calling thread's whether a log line is wanted, so a line
    // another test's thread reaches first would be cached as unwanted here too. With two, it asks both.
    let _second = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .finish(),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    endpoint.status.store(401, Ordering::SeqCst);
    // A closed port too: a connection error must not quote the URL either.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_address = closed.local_addr().unwrap().to_string();
    drop(closed);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let text = format!(
        "{}\n[[notify.targets]]\nname = \"desk\"\nkind = \"ntfy\"\nurl = \"http://{closed_address}/s3cret-topic\"\n",
        webhook(&format!("http://{}/hook?key=s3cret", endpoint.address))
    );
    let config = config_text(&dir, &text);
    let notifier = Running::start(&store, &config).await;
    store.event(failed(1));
    endpoint.wait_for(2).await;
    notifier.stop().await;
    let _ = captured.clone().flush();

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("notify phone: not delivered: the target answered HTTP 401 Unauthorized"),
        "{logs}"
    );
    assert!(
        logs.contains("notify desk: not delivered: cannot connect to the target"),
        "{logs}"
    );
    for secret in [
        "s3cret",
        "hunter2",
        endpoint.address.as_str(),
        closed_address.as_str(),
        "/hook",
    ] {
        assert!(!logs.contains(secret), "a log line shows {secret}");
    }
}

#[test]
fn the_digest_goes_once_a_day_after_its_local_time() {
    const HOUR: u64 = 3600;
    let midnight = 1_799_971_200 - 1_799_971_200 % (24 * HOUR);
    let nine = DayTime { minutes: 9 * 60 };
    let paris = parse_offset("+0100").unwrap();
    assert_eq!(paris, 3600);
    assert_eq!(parse_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));

    // 08:30 in Paris is 07:30 UTC: not yet.
    assert!(!digest_due(midnight + 7 * HOUR + 1800, paris, nine, 0));
    // 09:00 in Paris, last sent yesterday: due.
    assert!(digest_due(
        midnight + 8 * HOUR,
        paris,
        nine,
        midnight - 15 * HOUR
    ));
    // Sent at 09:00; at 15:00 it is not due again.
    assert!(!digest_due(
        midnight + 14 * HOUR,
        paris,
        nine,
        midnight + 8 * HOUR
    ));
    // Sent at 09:00, then moved to 16:00: not a second today.
    assert!(!digest_due(
        midnight + 15 * HOUR,
        paris,
        DayTime { minutes: 16 * 60 },
        midnight + 8 * HOUR
    ));
}

#[tokio::test]
async fn a_target_that_redirects_is_not_counted_as_delivered() {
    // A proxy in front of ntfy that sends http to https: a POST followed there would arrive as a GET without its
    // body, which the server's web page answers with 200.
    let (dir, received) = (Dir::new(), Received::default());
    let (posts, gets) = (received.clone(), received.clone());
    let app = Router::new()
        .route(
            "/hook",
            post(move || {
                let posts = posts.clone();
                async move {
                    posts.lock().unwrap().push((
                        "/hook".to_owned(),
                        HeaderMap::new(),
                        Vec::new(),
                        301,
                    ));
                    (StatusCode::MOVED_PERMANENTLY, [("location", "/moved")])
                }
            }),
        )
        .route(
            "/moved",
            axum::routing::get(move || {
                let gets = gets.clone();
                async move {
                    gets.lock().unwrap().push((
                        "/moved".to_owned(),
                        HeaderMap::new(),
                        Vec::new(),
                        200,
                    ));
                    StatusCode::OK
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let config = config_text(&dir, &webhook(&format!("http://{address}/hook")));
    let notifier = Running::start(&store, &config).await;

    store.event(failed(1));
    for _ in 0..200 {
        if received.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    notifier.stop().await;

    let paths: Vec<String> = received
        .lock()
        .unwrap()
        .iter()
        .map(|post| post.0.clone())
        .collect();
    assert!(
        paths.len() >= 2 && paths.iter().all(|path| path == "/hook"),
        "the failure is posted again, never followed to a GET: {paths:?}"
    );
}

#[tokio::test]
async fn an_insight_told_before_a_long_downtime_is_not_told_again_while_it_holds() {
    const HOUR: u64 = 3_600_000;
    let timing = Timing {
        gather: Duration::ZERO,
        gap: Duration::ZERO,
        ..SETUP.timing
    };
    let (dir, endpoint) = (Dir::new(), Endpoint::start().await);
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let config = Config::parse(&webhook(&format!("http://{}/hook", endpoint.address))).unwrap();
    let phone = &config.notify.targets[0];
    let http = client(timing.timeout).unwrap();
    let mut target = start(&store, None, phone, &config).await.unwrap();
    let t0 = 1_800_000_000_000;
    target.look(&[flaky()], t0);
    send_due(&store, &http, &timing, &mut target).await;
    assert_eq!(endpoint.count(), 1, "told once");

    // The host is off for a weekend; the job is still flaky when the daemon starts again.
    let kept = store.notify_state().await.unwrap().remove("phone");
    let mut target = start(&store, kept, phone, &config).await.unwrap();
    target.look(&[flaky()], t0 + 60 * HOUR);
    send_due(&store, &http, &timing, &mut target).await;
    assert_eq!(
        endpoint.count(),
        1,
        "a daemon that was off did not see it go away"
    );
}
