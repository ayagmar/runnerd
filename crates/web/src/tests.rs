//! The dashboard against a fake daemon on a real Unix socket: who gets in, and that every page renders from API
//! data.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("runnerd-web-test-{}-{n}", std::process::id()));
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

#[path = "../examples/demo/fake.rs"]
mod fake;

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use runnerd_core::api::{Event, EventEnvelope, HostSample, Sample};

/// 2026-10-22T14:03:07Z.
const NOW: u64 = 1_792_677_787;
const LEDGER_RUNNING: u64 = 120_055_991_700;

/// The dashboard serving a fake daemon's fixtures, and a browser for it.
struct Dashboard {
    addr: SocketAddr,
    dir: TempDir,
    fake: fake::Fake,
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Dashboard {
    async fn start() -> Dashboard {
        let dir = TempDir::new();
        let socket = dir.0.join("runnerd.sock");
        let fake = fake::Fake::new(fake::Fixtures::new(NOW));
        fake::serve(&socket, fake.clone());
        let listener = crate::listen("127.0.0.1:0").await.unwrap().unwrap();
        let addr = listener.local_addr().unwrap();
        crate::spawn(listener, socket, std::future::pending::<()>());
        // The login file is written before the first request is served.
        for _ in 0..100 {
            if crate::login_file(&dir.0.join("runnerd.sock")).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Dashboard { addr, dir, fake }
    }

    fn login_url(&self) -> String {
        std::fs::read_to_string(crate::login_file(&self.dir.0.join("runnerd.sock")))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// Signs in with the one-time URL and returns the session cookie.
    async fn sign_in(&self) -> String {
        let url = self.login_url();
        let path = url.strip_prefix(&format!("http://{}", self.addr)).unwrap();
        let reply = self.request("GET", path, &[]).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        let cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        cookie.split(';').next().unwrap().to_owned()
    }

    async fn connect(&self) -> hyper::client::conn::http1::SendRequest<Full<Bytes>> {
        let stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        sender
    }

    fn build(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
        self.build_with(method, path, headers, Bytes::new())
    }

    fn build_with(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Bytes,
    ) -> Request<Full<Bytes>> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, self.addr.to_string());
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(Full::new(body)).unwrap()
    }

    async fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
        self.send(self.build(method, path, headers)).await
    }

    /// A write from the dashboard's own page, as Datastar sends it: the cookie, the origin, the signals as JSON.
    async fn write(
        &self,
        cookie: &str,
        method: &str,
        path: &str,
        signals: &serde_json::Value,
    ) -> Reply {
        let origin = format!("http://{}", self.addr);
        let headers = [
            ("cookie", cookie),
            ("origin", origin.as_str()),
            ("content-type", "application/json"),
            ("datastar-request", "true"),
        ];
        let body = Bytes::from(signals.to_string());
        self.send(self.build_with(method, path, &headers, body))
            .await
    }

    async fn send(&self, request: Request<Full<Bytes>>) -> Reply {
        let mut sender = self.connect().await;
        let response = sender.send_request(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8(body.to_vec()).unwrap(),
        }
    }

    async fn page(&self, cookie: &str, path: &str) -> Reply {
        self.request("GET", path, &[("cookie", cookie)]).await
    }

    /// Opens a live stream and returns a reader of its body.
    async fn stream(&self, cookie: &str, path: &str) -> hyper::body::Incoming {
        let mut sender = self.connect().await;
        let response = sender
            .send_request(self.build("GET", path, &[("cookie", cookie)]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // The connection lives as long as the stream is read.
        std::mem::forget(sender);
        response.into_body()
    }
}

/// Reads the stream until `found` holds for what arrived, or fails after 5 s.
async fn read_until(body: &mut hyper::body::Incoming, found: impl Fn(&str) -> bool) -> String {
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !found(&text) {
            let frame = body.frame().await.unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the stream never sent what was expected; it sent:\n{text}"));
    text
}

#[tokio::test]
async fn without_the_session_cookie_every_page_and_write_is_401() {
    let dashboard = Dashboard::start().await;
    for (method, path) in [
        ("GET", "/"),
        ("GET", "/jobs/1"),
        ("GET", "/live/overview"),
        ("POST", "/v1/repositories/x/pause"),
        ("POST", "/v1/repositories/halyard/ledger/pause"),
    ] {
        let reply = dashboard.request(method, path, &[]).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    let wrong = dashboard
        .request(
            "POST",
            "/v1/repositories/halyard/ledger/pause",
            &[("cookie", "runnerd-1=0")],
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(!dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused);
    // The assets need no session: the sign-in page uses them.
    let style = dashboard.request("GET", "/assets/style.css", &[]).await;
    assert_eq!(style.status, StatusCode::OK);
}

#[tokio::test]
async fn a_write_from_another_origin_is_403_even_with_the_cookie() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    for origin in [
        Some("http://evil.example"),
        Some("http://127.0.0.1:1"),
        None,
    ] {
        let mut headers = vec![("cookie", cookie.as_str())];
        if let Some(origin) = origin {
            headers.push(("origin", origin));
        }
        let reply = dashboard
            .request("POST", "/v1/repositories/halyard/ledger/pause", &headers)
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "origin {origin:?}");
    }
    assert!(!dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused);
}

#[tokio::test]
async fn a_resume_from_the_dashboard_reaches_the_daemon_and_returns_the_row_with_pause() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let origin = format!("http://{}", dashboard.addr);
    dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused = true;
    let reply = dashboard
        .request(
            "POST",
            "/v1/repositories/halyard/ledger/resume",
            &[("cookie", &cookie), ("origin", &origin)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused);
    assert!(
        reply.body.contains(r#"id="repo-halyard-ledger""#),
        "{}",
        reply.body
    );
    assert!(
        reply.body.contains(">Pause<") && !reply.body.contains("badge neutral"),
        "{}",
        reply.body
    );
    // A repository the daemon does not know comes back as a notice Datastar shows.
    let unknown = dashboard
        .request(
            "POST",
            "/v1/repositories/halyard/nope/pause",
            &[("cookie", &cookie), ("origin", &origin)],
        )
        .await;
    assert!(
        unknown.body.contains(r#"id="notice""#)
            && unknown.body.contains("not a configured repository")
    );
}

#[tokio::test]
async fn a_login_link_signs_in_once() {
    let dashboard = Dashboard::start().await;
    let used = dashboard.login_url();
    dashboard.sign_in().await;
    let path = used
        .strip_prefix(&format!("http://{}", dashboard.addr))
        .unwrap();
    let again = dashboard.request("GET", path, &[]).await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
    assert_ne!(dashboard.login_url(), used);
}

#[tokio::test]
async fn every_page_renders_from_the_daemons_data() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await;
    assert_eq!(overview.status, StatusCode::OK);
    for expected in [
        "ci.slice",
        "halyard/ledger",
        "halyard/storefront",
        "Integration tests (Testcontainers)",
        "MemoryMax 33G",
        "Sampled peak",
    ] {
        assert!(
            overview.body.contains(expected),
            "overview lacks {expected:?}"
        );
    }
    let csp = overview.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'none'") && csp.contains("connect-src 'self'"));

    let job = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}"))
        .await;
    assert_eq!(job.status, StatusCode::OK);
    for expected in [
        "Set up JDK 21",
        "jvm now 9G",
        "jvm now 8 CPUs",
        "sampled peak",
        "Waiting for a runner",
        "Set up job: line 1",
        "/live/jobs/",
    ] {
        assert!(job.body.contains(expected), "job page lacks {expected:?}");
    }
    // The API has the profile's limits as configured now, not the caps the job's container had.
    assert!(
        !job.body.contains("slot cap"),
        "the job page presents today's config as the job's caps"
    );

    let history = dashboard.page(&cookie, "/history").await;
    assert_eq!(history.status, StatusCode::OK);
    assert!(history.body.contains("Jobs per day") && history.body.contains("playwright"));

    let caches = dashboard.page(&cookie, "/caches").await;
    assert_eq!(caches.status, StatusCode::OK);
    assert!(caches.body.contains("runnerd-pnpm") && caches.body.contains("emptied above 5G"));

    let log = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}/log"))
        .await;
    assert!(log.body.starts_with("##[group]Set up job\n"));

    let missing = dashboard.page(&cookie, "/jobs/7").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn history_shows_only_the_repository_and_outcome_filtered_for() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let failed = dashboard
        .page(
            &cookie,
            "/history?repository=halyard%2Fledger&outcome=failure",
        )
        .await;
    let rows: Vec<&str> = failed.body.split("<tr class=").skip(1).collect();
    assert_ne!(rows.len(), 0);
    for row in &rows {
        assert!(
            row.contains("halyard/ledger") && row.contains("badge failure"),
            "{row}"
        );
    }
    let everything = dashboard
        .page(&cookie, "/history?repository=&outcome=")
        .await;
    assert!(everything.body.split("<tr class=").count() > rows.len() + 1);
}

#[tokio::test]
async fn the_overview_stream_patches_the_slice_when_a_sample_arrives() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    let first = read_until(&mut stream, |text| text.contains(r#"id="recent""#)).await;
    assert!(first.contains("event: datastar-patch-elements") && first.contains(r#"id="budget""#));
    let at = NOW + 2;
    let sample = HostSample {
        at,
        cpu: 31_000,
        memory: 21 << 30,
    };
    dashboard
        .fake
        .fixtures
        .lock()
        .unwrap()
        .host
        .push(sample.clone());
    dashboard
        .fake
        .events
        .send(EventEnvelope {
            id: None,
            at,
            event: Event::HostSample { sample },
        })
        .unwrap();
    let update = read_until(&mut stream, |text| text.contains(r#"id="host""#)).await;
    assert!(update.contains("31 cores"), "{update}");
}

#[tokio::test]
async fn a_running_jobs_stream_appends_new_log_lines_and_updates_its_charts() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let lines = dashboard.fake.fixtures.lock().unwrap().logs[&LEDGER_RUNNING].len() as u64;
    let mut stream = dashboard
        .stream(
            &cookie,
            &format!("/live/jobs/{LEDGER_RUNNING}?from={lines}"),
        )
        .await;
    read_until(&mut stream, |text| text.contains(r#"id="metrics""#)).await;
    let at = NOW + 2;
    for event in [
        Event::LogLine {
            job: LEDGER_RUNNING,
            line: lines,
            text: "BUILD SUCCESSFUL in 2m 28s".to_owned(),
        },
        Event::JobSample {
            job: LEDGER_RUNNING,
            sample: Sample {
                at,
                cpu: 7000,
                memory: 8 << 30,
                network_received: 0,
                network_sent: 0,
            },
        },
    ] {
        dashboard
            .fake
            .events
            .send(EventEnvelope {
                id: None,
                at,
                event,
            })
            .unwrap();
    }
    let update = read_until(&mut stream, |text| {
        text.contains("BUILD SUCCESSFUL") && text.contains(r#"id="metrics""#)
    })
    .await;
    assert!(
        update.contains("selector #log-lines") && update.contains("mode append"),
        "{update}"
    );
    assert!(update.contains("sampled peak 8G"), "{update}");
}

/// The log lines a browser shows, as `(number, text)`: what the job page rendered, then each of the job stream's
/// patches to `#log-lines` applied the way Datastar applies them.
struct ShownLog(Vec<(Option<u64>, String)>);

impl ShownLog {
    /// The `<li value="N">text</li>` items in `html`; the number is `None` on an `<li>` without one.
    fn items(html: &str) -> Vec<(Option<u64>, String)> {
        html.split("<li")
            .skip(1)
            .map(|item| {
                let (attributes, rest) = item.split_once('>').unwrap();
                let number = attributes
                    .split_once("value=\"")
                    .map(|(_, value)| value.trim_end_matches('"').parse().unwrap());
                (number, rest.split_once("</li>").unwrap().0.to_owned())
            })
            .collect()
    }

    fn of_page(page: &str) -> ShownLog {
        let list = page.split_once(r#"id="log-lines""#).unwrap().1;
        ShownLog(Self::items(list.split_once("</ol>").unwrap().0))
    }

    /// Applies the patches to `#log-lines` among the stream's `events`.
    fn apply(&mut self, events: &str) {
        for event in events.split("\n\n") {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect();
            let field = |name: &str| {
                data.iter()
                    .find_map(|line| line.strip_prefix(name).map(str::trim))
            };
            let elements: String = data
                .iter()
                .filter_map(|line| line.strip_prefix("elements "))
                .collect();
            match (field("selector "), field("mode ")) {
                (Some("#log-lines"), Some("append")) => self.0.extend(Self::items(&elements)),
                (Some(selector), Some("remove")) => {
                    // The only removal the stream sends: all but the last `keep` lines.
                    let keep: usize = selector
                        .strip_prefix("#log-lines > li:nth-last-child(n+")
                        .and_then(|rest| rest.strip_suffix(')'))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                        - 1;
                    let excess = self.0.len().saturating_sub(keep);
                    self.0.drain(..excess);
                }
                _ if elements.contains(r#"id="log-lines""#) => {
                    self.0 = ShownLog::of_page(&elements).0;
                }
                _ => {}
            }
        }
    }
}

impl Dashboard {
    /// Adds a line to the running job's log, as the daemon stores and announces it.
    fn log(&self, text: &str) {
        let mut fixtures = self.fake.fixtures.lock().unwrap();
        let lines = fixtures.logs.entry(LEDGER_RUNNING).or_default();
        let line = lines.len() as u64;
        lines.push(text.to_owned());
        let job = fixtures
            .jobs
            .iter_mut()
            .find(|job| job.summary.id == LEDGER_RUNNING)
            .unwrap();
        job.summary.log.lines = line + 1;
        let _ = self.fake.events.send(EventEnvelope {
            id: None,
            at: NOW,
            event: Event::LogLine {
                job: LEDGER_RUNNING,
                line,
                text: text.to_owned(),
            },
        });
    }

    /// The job page and the URL its live stream connects to, as the page names it.
    async fn job_page(&self, cookie: &str) -> (String, String) {
        let page = self
            .page(cookie, &format!("/jobs/{LEDGER_RUNNING}"))
            .await
            .body;
        let url = page
            .split_once("@get(&#39;")
            .or_else(|| page.split_once("@get('"))
            .unwrap()
            .1
            .split(['&', '\''])
            .next()
            .unwrap()
            .to_owned();
        (page, url)
    }
}

#[tokio::test]
async fn a_job_left_open_on_a_noisy_log_keeps_only_its_last_lines() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let (page, url) = dashboard.job_page(&cookie).await;
    let mut shown = ShownLog::of_page(&page);
    let mut stream = dashboard.stream(&cookie, &url).await;
    let mut events = read_until(&mut stream, |text| text.contains(r#"id="metrics""#)).await;
    let tail = usize::try_from(crate::views::job::LOG_TAIL).unwrap();
    // In batches, so the fake's event channel never drops one.
    for batch in 0..6 {
        let last = format!("noisy line {}", batch * 200 + 199);
        for n in batch * 200..batch * 200 + 200 {
            dashboard.log(&format!("noisy line {n}"));
        }
        events.push_str(&read_until(&mut stream, |text| text.contains(&last)).await);
    }
    // A sample after the last line: its chart comes after whatever the stream sends for the lines.
    let _ = dashboard.fake.events.send(EventEnvelope {
        id: None,
        at: NOW + 2,
        event: Event::JobSample {
            job: LEDGER_RUNNING,
            sample: Sample {
                at: NOW + 2,
                cpu: 1000,
                memory: 13 << 30,
                network_received: 0,
                network_sent: 0,
            },
        },
    });
    events.push_str(&read_until(&mut stream, |text| text.contains("sampled peak 13G")).await);
    shown.apply(&events);
    assert!(
        shown.0.len() <= tail,
        "the page shows {} lines",
        shown.0.len()
    );
    assert_eq!(
        shown.0.last().map(|line| line.1.as_str()),
        Some("noisy line 1199")
    );
}

#[tokio::test]
async fn the_dashboard_refuses_to_listen_beyond_loopback() {
    let error = crate::listen("0.0.0.0:0").await.unwrap_err();
    assert!(error.to_string().contains("loopback only"));
    assert!(crate::listen("off").await.unwrap().is_none());
}

/// The fixtures' config with jvm's memory raised from 9G to `memory`.
fn with_jvm_memory(dashboard: &Dashboard, memory: &str) -> String {
    let config = dashboard.fake.fixtures.lock().unwrap().config.clone();
    let raised = config.replacen("memory = \"9G\"", &format!("memory = \"{memory}\""), 1);
    assert_ne!(raised, config);
    raised
}

#[tokio::test]
async fn a_config_over_the_budget_is_not_saved_and_the_page_shows_both_numbers() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let before = dashboard.fake.fixtures.lock().unwrap().config.clone();
    // 2 × 12G + 2 × 7G + 512M = 38.5G, over ci.slice's 33G.
    let toml = with_jvm_memory(&dashboard, "12G");
    let reply = dashboard
        .write(
            &cookie,
            "PUT",
            "/config",
            &serde_json::json!({ "toml": toml }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.contains(r#"id="config-result""#) && reply.body.contains("Not saved"),
        "{}",
        reply.body
    );
    assert!(
        reply.body.contains("38.5G") && reply.body.contains("33G"),
        "{}",
        reply.body
    );
    assert_eq!(dashboard.fake.fixtures.lock().unwrap().config, before);
}

#[tokio::test]
async fn a_config_that_fits_is_saved_and_checking_one_saves_nothing() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let toml = with_jvm_memory(&dashboard, "8G");
    let checked = dashboard
        .write(
            &cookie,
            "POST",
            "/config/check",
            &serde_json::json!({ "toml": toml }),
        )
        .await;
    assert!(checked.body.contains("Fits the budget"), "{}", checked.body);
    assert_ne!(dashboard.fake.fixtures.lock().unwrap().config, toml);
    let saved = dashboard
        .write(
            &cookie,
            "PUT",
            "/config",
            &serde_json::json!({ "toml": toml }),
        )
        .await;
    assert!(saved.body.contains("Saved and reloaded"), "{}", saved.body);
    assert_eq!(dashboard.fake.fixtures.lock().unwrap().config, toml);
}

#[tokio::test]
async fn the_config_page_shows_the_file_its_budget_and_the_doctor() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let page = dashboard.page(&cookie, "/config").await;
    assert_eq!(page.status, StatusCode::OK);
    for expected in [
        "halyard/ledger",
        "/home/forge/.config/runnerd/runnerd.toml",
        "Fits the budget",
        "jvm 2",
        "MemoryHigh=30G",
    ] {
        assert!(
            page.body.contains(expected),
            "config page lacks {expected:?}"
        );
    }
}

#[tokio::test]
async fn pausing_a_repository_with_a_running_job_asks_first_and_cancels_only_when_told() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let asked = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/repositories/halyard/ledger/pause",
            &serde_json::json!({}),
        )
        .await;
    assert!(
        asked.body.contains("Cancel and pause") && asked.body.contains("pause?force=true"),
        "{}",
        asked.body
    );
    assert!(
        !asked.body.contains("pause with force"),
        "the question asks rather than repeating the CLI's hint: {}",
        asked.body
    );
    assert!(
        asked.body.contains(&LEDGER_RUNNING.to_string()),
        "the question names the job: {}",
        asked.body
    );
    assert!(!dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused);
    let forced = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/repositories/halyard/ledger/pause?force=true",
            &serde_json::json!({}),
        )
        .await;
    assert!(
        forced.body.contains(r#"id="repo-halyard-ledger""#) && forced.body.contains("stopped job"),
        "{}",
        forced.body
    );
    let fixtures = dashboard.fake.fixtures.lock().unwrap();
    assert!(fixtures.status.repositories[0].paused);
    assert_eq!(
        fixtures.controls,
        [
            "pause halyard/ledger force=false",
            "pause halyard/ledger force=true"
        ]
    );
}

#[tokio::test]
async fn reseed_and_empty_from_the_caches_page_reach_the_daemon() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let page = dashboard.page(&cookie, "/caches").await;
    assert!(
        page.body.contains("/v1/profiles/jvm/reseed")
            && page.body.contains("/v1/caches/runnerd-pnpm/clean")
    );
    let reseed = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/profiles/jvm/reseed",
            &serde_json::json!({}),
        )
        .await;
    assert!(reseed.body.contains("Reseeding jvm"), "{}", reseed.body);
    let clean = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/caches/runnerd-pnpm/clean",
            &serde_json::json!({}),
        )
        .await;
    assert!(
        clean.body.contains("Emptied runnerd-pnpm"),
        "{}",
        clean.body
    );
    assert_eq!(
        dashboard.fake.fixtures.lock().unwrap().controls,
        ["reseed jvm", "clean runnerd-pnpm"]
    );
}
