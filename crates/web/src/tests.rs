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
        // Only this user may write to it, as the dashboard's session secret needs.
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
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
use runnerd_core::api::{
    Event, EventEnvelope, HostSample, JobState, Sample, SeedMemory, ShellMemory, Waiting,
};

use crate::views::{repository_id, slot_id};

/// 2026-10-22T14:03:07Z.
const NOW: u64 = 1_792_677_787;
const LEDGER_RUNNING: u64 = 120_055_991_700;

/// The dashboard serving a fake daemon's fixtures, and a browser for it.
struct Dashboard {
    addr: SocketAddr,
    dir: TempDir,
    fake: fake::Fake,
    /// The credentials it checks, as the daemon's socket routes share them.
    sessions: std::sync::Arc<crate::Sessions>,
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Dashboard {
    /// Sets the caps the running job's record says its container ran with.
    fn runner_caps(&self, memory: Option<u64>, cpus: Option<u64>) {
        let mut fixtures = self.fake.fixtures.lock().unwrap();
        let job = fixtures
            .jobs
            .iter_mut()
            .find(|job| job.summary.id == LEDGER_RUNNING)
            .unwrap();
        let runner = job.summary.runner.as_mut().unwrap();
        runner.memory_cap = memory;
        runner.cpu_cap = cpus;
    }

    async fn start() -> Dashboard {
        Dashboard::behind(&[]).await
    }

    /// The dashboard that also takes writes from the pages of `proxies`, as `[web] origins` sets.
    async fn behind(proxies: &[&str]) -> Dashboard {
        let proxies = proxies
            .iter()
            .map(|origin| origin.parse().unwrap())
            .collect();
        let dir = TempDir::new();
        let socket = dir.0.join("runnerd.sock");
        let fake = fake::Fake::new(fake::Fixtures::new(NOW));
        fake::serve(&socket, fake.clone());
        let listener = crate::listen("127.0.0.1:0").await.unwrap().unwrap();
        let addr = listener.local_addr().unwrap();
        let sessions = crate::Sessions::open(dir.0.join("dashboard-session")).unwrap();
        crate::spawn(
            listener,
            socket,
            proxies,
            sessions.clone(),
            std::future::pending::<()>(),
        );
        // The login file is written before the first request is served.
        for _ in 0..100 {
            if crate::login_file(&dir.0.join("runnerd.sock")).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Dashboard {
            addr,
            dir,
            fake,
            sessions,
        }
    }

    /// The login file's links: the loopback address's, then each proxy's.
    fn login_urls(&self) -> Vec<String> {
        std::fs::read_to_string(crate::login_file(&self.dir.0.join("runnerd.sock")))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn login_url(&self) -> String {
        self.login_urls().remove(0)
    }

    /// Signs in with the one-time URL and returns the session cookie.
    async fn sign_in(&self) -> String {
        let url = self.login_url();
        let path = url.strip_prefix(&format!("http://{}", self.addr)).unwrap();
        let reply = self.request("GET", path, &[]).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        let cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
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
        let mut request = Request::builder().method(method).uri(path);
        // The dashboard's own address, unless the request comes through a proxy that names another.
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("host"))
        {
            request = request.header(header::HOST, self.addr.to_string());
        }
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
        ("GET", "/sizing"),
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
async fn an_asset_is_kept_until_it_changes_and_revalidates_without_its_body() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let page = dashboard.page(&cookie, "/").await.body;
    let from = page.find("/assets/style.css?v=").unwrap();
    let url = &page[from..from + page[from..].find('"').unwrap()];
    let style = dashboard.request("GET", url, &[]).await;
    assert_eq!(
        style.headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let etag = style.headers[header::ETAG].to_str().unwrap().to_owned();
    let again = dashboard
        .request("GET", url, &[("if-none-match", &etag)])
        .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty(), "a 304 has no body");
    // An older release's URL may name other bytes: checked each time.
    let old = dashboard
        .request("GET", "/assets/style.css?v=0.0.5", &[])
        .await;
    assert_eq!(old.headers[header::CACHE_CONTROL], "no-cache");
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
async fn a_link_from_another_site_opens_the_page_signed_in_and_a_write_from_it_is_still_refused() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    // Following a link on GitHub: the browser sends a `SameSite=Lax` cookie on a top-level GET.
    let navigation = [
        ("cookie", cookie.as_str()),
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-mode", "navigate"),
    ];
    let page = dashboard
        .request("GET", "/jobs/120055991700", &navigation)
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(!page.body.contains("Sign in to runnerd"), "{}", page.body);
    // A form on that site posting to the dashboard, cookie and all: its `Origin` is not the dashboard's.
    let write = [
        ("cookie", cookie.as_str()),
        ("origin", "https://github.com"),
        ("sec-fetch-site", "cross-site"),
    ];
    let reply = dashboard
        .request("POST", "/v1/repositories/halyard/ledger/pause", &write)
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(
        !dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused,
        "halyard/ledger stays running"
    );
}

#[tokio::test]
async fn a_write_through_a_configured_proxy_reaches_the_daemon_and_others_stay_403() {
    let dashboard = Dashboard::behind(&["https://forge.example.ts.net:8443"]).await;
    let cookie = dashboard.sign_in().await;
    dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused = true;
    let resume = |origin: &'static str| {
        let headers = [("cookie", cookie.as_str()), ("origin", origin)];
        let request = dashboard.build("POST", "/v1/repositories/halyard/ledger/resume", &headers);
        dashboard.send(request)
    };
    for origin in [
        "https://forge.example.ts.net",
        "http://forge.example.ts.net:8443",
        "https://evil.example.ts.net:8443",
    ] {
        assert_eq!(
            resume(origin).await.status,
            StatusCode::FORBIDDEN,
            "origin {origin}"
        );
    }
    assert!(
        dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused,
        "halyard/ledger stays paused"
    );
    let reply = resume("https://forge.example.ts.net:8443").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(
        !dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused,
        "halyard/ledger runs again"
    );
}

#[tokio::test]
async fn a_proxy_configured_by_a_long_address_takes_writes_with_the_origin_a_browser_sends() {
    let dashboard = Dashboard::behind(&["http://[0:0:0:0:0:0:0:1]:8443"]).await;
    let cookie = dashboard.sign_in().await;
    dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused = true;
    // `new URL("http://[0:0:0:0:0:0:0:1]:8443").origin` in a browser.
    let headers = [("cookie", cookie.as_str()), ("origin", "http://[::1]:8443")];
    let reply = dashboard
        .send(dashboard.build("POST", "/v1/repositories/halyard/ledger/resume", &headers))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(
        !dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused,
        "halyard/ledger runs again"
    );
}

#[tokio::test]
async fn a_login_through_an_https_proxy_sets_a_secure_cookie_and_a_loopback_one_does_not() {
    let dashboard = Dashboard::behind(&["https://forge.example.ts.net:8443"]).await;
    let mut cookies = Vec::new();
    for headers in [vec![], vec![("x-forwarded-proto", "https")]] {
        let url = dashboard.login_url();
        let path = url
            .strip_prefix(&format!("http://{}", dashboard.addr))
            .unwrap();
        let reply = dashboard.request("GET", path, &headers).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        cookies.push(
            reply.headers[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert!(!cookies[0].contains("Secure"), "{}", cookies[0]);
    assert!(
        cookies[1].ends_with("; Secure") && cookies[1].contains("SameSite=Lax"),
        "{}",
        cookies[1]
    );
}

#[tokio::test]
async fn every_page_links_a_manifest_a_browser_can_install_without_a_session() {
    let dashboard = Dashboard::start().await;
    let signed_out = dashboard.request("GET", "/", &[]).await;
    let cookie = dashboard.sign_in().await;
    for page in [
        signed_out,
        dashboard.page(&cookie, "/").await,
        dashboard.page(&cookie, "/nowhere").await,
    ] {
        let link = r#"<link rel="manifest" href=""#;
        let from = page.body.find(link).unwrap() + link.len();
        let url = &page.body[from..from + page.body[from..].find('"').unwrap()];
        assert!(
            page.headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("manifest-src 'self'"),
            "the policy lets the page load its manifest"
        );
        // A browser fetches the manifest and its icons without cookies.
        let manifest = dashboard.request("GET", url, &[]).await;
        assert_eq!(manifest.status, StatusCode::OK);
        assert_eq!(
            manifest.headers[header::CONTENT_TYPE],
            "application/manifest+json"
        );
        let manifest: serde_json::Value = serde_json::from_str(&manifest.body).unwrap();
        assert_eq!(manifest["start_url"], "/");
        assert_eq!(manifest["display"], "standalone");
        assert!(manifest["name"].is_string(), "the manifest names the app");
        let icons = manifest["icons"].as_array().unwrap();
        for size in ["192x192", "512x512"] {
            assert!(
                icons
                    .iter()
                    .any(|icon| icon["sizes"] == size && icon["purpose"] == "any"),
                "an icon of {size}, as Chrome needs to install"
            );
        }
        for icon in icons {
            let src = icon["src"].as_str().unwrap();
            let reply = dashboard.request("HEAD", src, &[]).await;
            assert_eq!(reply.status, StatusCode::OK, "{src}");
            assert_eq!(reply.headers[header::CONTENT_TYPE], "image/png", "{src}");
        }
    }
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
        reply
            .body
            .contains(&format!(r#"id="{}""#, repository_id("halyard/ledger"))),
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
async fn repositories_whose_names_differ_only_in_punctuation_keep_their_own_rows() {
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let mut dash = fixtures.status.repositories[0].clone();
        dash.name = "halyard/b-c".to_owned();
        dash.paused = true;
        let mut underscore = dash.clone();
        underscore.name = "halyard/b_c".to_owned();
        fixtures.status.repositories = vec![dash, underscore];
    }
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await.body;
    let (dash, underscore) = (repository_id("halyard/b-c"), repository_id("halyard/b_c"));
    for id in [&dash, &underscore] {
        assert_eq!(
            overview.matches(&format!(r#"id="{id}""#)).count(),
            1,
            "one row has the id {id}"
        );
        let busy = format!(r#"data-indicator="_busy_{}""#, id.replace('-', "_"));
        assert_eq!(
            overview.matches(&busy).count(),
            1,
            "{id}'s pause button has its own busy signal"
        );
    }
    // The resume's reply replaces the row with its id: it must be that repository's.
    let origin = format!("http://{}", dashboard.addr);
    let reply = dashboard
        .request(
            "POST",
            "/v1/repositories/halyard/b_c/resume",
            &[("cookie", &cookie), ("origin", &origin)],
        )
        .await;
    assert!(
        reply.body.contains(&format!(r#"id="{underscore}""#))
            && !reply.body.contains(&format!(r#"id="{dash}""#)),
        "{}",
        reply.body
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
async fn a_browser_that_opens_its_login_link_again_lands_signed_in() {
    let dashboard = Dashboard::start().await;
    let used = dashboard.login_url();
    let cookie = dashboard.sign_in().await;
    let path = used
        .strip_prefix(&format!("http://{}", dashboard.addr))
        .unwrap();
    let again = dashboard
        .request("GET", path, &[("cookie", cookie.as_str())])
        .await;
    assert_eq!(again.status, StatusCode::SEE_OTHER);
    assert_eq!(again.headers[header::LOCATION], "/");
}

/// The one-time link in a QR code's reply.
fn pairing_link(reply: &str) -> &str {
    let from = reply.find("login?code=").unwrap();
    let from = reply[..from].rfind('>').unwrap() + 1;
    &reply[from..from + reply[from..].find('<').unwrap()]
}

#[tokio::test]
async fn a_qr_code_from_the_config_page_signs_a_phone_in_through_the_proxy_once() {
    let dashboard = Dashboard::behind(&["https://forge.example.ts.net:8443"]).await;
    let cookie = dashboard.sign_in().await;
    let config = dashboard.page(&cookie, "/config").await.body;
    assert!(
        config.contains("Add a device") && config.contains("@post('/pair')"),
        "the config page offers a QR code"
    );
    let reply = dashboard
        .write(&cookie, "POST", "/pair", &serde_json::json!({}))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.contains(r#"id="pairing-code""#)
            && reply.body.contains("<svg class=\"qr-code\""),
        "{}",
        reply.body
    );
    let link = pairing_link(&reply.body);
    let path = link
        .strip_prefix("https://forge.example.ts.net:8443")
        .unwrap();
    // `tailscale serve` passes the phone's Host on and says it took the request over https.
    let phone = [
        ("x-forwarded-proto", "https"),
        ("host", "forge.example.ts.net:8443"),
    ];
    let elsewhere = dashboard.request("GET", path, &[]).await;
    assert_eq!(
        elsewhere.status,
        StatusCode::UNAUTHORIZED,
        "the code does not work on the loopback address"
    );
    let signed_in = dashboard.request("GET", path, &phone).await;
    assert_eq!(signed_in.status, StatusCode::SEE_OTHER);
    let set = signed_in.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(set.contains("; Secure"), "the phone's cookie is Secure");
    let phone_cookie = set.split(';').next().unwrap();
    assert_eq!(
        dashboard.page(phone_cookie, "/").await.status,
        StatusCode::OK
    );
    let again = dashboard.request("GET", path, &phone).await;
    assert_eq!(
        again.status,
        StatusCode::UNAUTHORIZED,
        "the code works once"
    );
}

#[tokio::test]
async fn a_qr_code_request_still_arriving_when_every_device_is_signed_out_gets_no_link() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let dashboard = Dashboard::behind(&["https://forge.example.ts.net:8443"]).await;
    let cookie = dashboard.sign_in().await;
    let mut stream = tokio::net::TcpStream::connect(dashboard.addr)
        .await
        .unwrap();
    // The headers pass the session check; the body is still on its way.
    let head = format!(
        "POST /pair HTTP/1.1\r\nhost: {}\r\ncookie: {cookie}\r\norigin: http://{}\r\n\
         content-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{",
        dashboard.addr, dashboard.addr
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    dashboard.sessions.sign_out_all().unwrap();
    stream.write_all(b"}").await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(
        reply.starts_with("HTTP/1.1 401"),
        "the request is signed out, not {}",
        reply.lines().next().unwrap_or_default()
    );
    assert!(!reply.contains("login?code="), "and gets no link");
}

#[tokio::test]
async fn a_qr_code_is_made_only_for_the_dashboards_own_pages() {
    let dashboard = Dashboard::behind(&["https://forge.example.ts.net:8443"]).await;
    let cookie = dashboard.sign_in().await;
    let without_session = dashboard.request("POST", "/pair", &[]).await;
    assert_eq!(without_session.status, StatusCode::UNAUTHORIZED);
    for origin in [None, Some("http://evil.example")] {
        let mut headers = vec![("cookie", cookie.as_str())];
        if let Some(origin) = origin {
            headers.push(("origin", origin));
        }
        let reply = dashboard.request("POST", "/pair", &headers).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "origin {origin:?}");
        assert!(!reply.body.contains("login?code="), "no link leaks");
    }
}

#[tokio::test]
async fn without_a_proxy_the_config_page_says_where_to_list_one() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let config = dashboard.page(&cookie, "/config").await.body;
    assert!(
        config.contains("[web] origins") && !config.contains("@post('/pair')"),
        "no QR code without an address a phone can reach"
    );
    let reply = dashboard
        .write(&cookie, "POST", "/pair", &serde_json::json!({}))
        .await;
    assert!(
        !reply.body.contains("login?code=") && reply.body.contains(r#"id="notice""#),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn the_signed_out_page_points_a_phone_at_the_qr_code_not_at_typing_an_address() {
    let dashboard = Dashboard::behind(&["https://forge.example.ts.net:8443"]).await;
    let page = dashboard
        .request("GET", "/", &[("x-forwarded-proto", "https")])
        .await
        .body;
    assert!(
        page.contains("Add a device") && page.contains("runnerctl web --qr"),
        "{page}"
    );
    assert!(!page.contains("--base"), "no address to type");
}

#[tokio::test]
async fn each_visit_renews_a_session_that_lasts_30_days() {
    let dashboard = Dashboard::start().await;
    let url = dashboard.login_url();
    let path = url
        .strip_prefix(&format!("http://{}", dashboard.addr))
        .unwrap();
    let login = dashboard.request("GET", path, &[]).await;
    let set = login.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(set.contains("; Max-Age=2592000;"), "{set}");
    let cookie = set.split(';').next().unwrap();
    let visit = dashboard.page(cookie, "/history").await;
    let renewed = visit.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(
        renewed.starts_with(cookie) && renewed.contains("; Max-Age=2592000;"),
        "the visit sets the cookie again with its full lifetime"
    );
}

#[tokio::test]
async fn a_browser_stays_signed_in_when_the_dashboard_restarts() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let secret = cookie.split_once('=').unwrap().1;
    // The same dashboard after a restart: a new listener, the same socket and session file.
    let listener = crate::listen("127.0.0.1:0").await.unwrap().unwrap();
    let addr = listener.local_addr().unwrap();
    let sessions = crate::Sessions::open(dashboard.dir.0.join("dashboard-session")).unwrap();
    crate::spawn(
        listener,
        dashboard.dir.0.join("runnerd.sock"),
        Vec::new(),
        sessions.clone(),
        std::future::pending::<()>(),
    );
    let restarted = Dashboard {
        addr,
        dir: TempDir::new(),
        fake: dashboard.fake.clone(),
        sessions: sessions.clone(),
    };
    // A browser sends the cookie it has for the dashboard's port, which stays the same across restarts.
    let cookie = format!("runnerd-{}={secret}", addr.port());
    assert_eq!(restarted.page(&cookie, "/").await.status, StatusCode::OK);
    sessions.sign_out_all().unwrap();
    assert_eq!(
        restarted.page(&cookie, "/").await.status,
        StatusCode::UNAUTHORIZED,
        "signing every device out ends it"
    );
}

#[tokio::test]
async fn every_page_renders_from_the_daemons_data() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await;
    assert_eq!(overview.status, StatusCode::OK);
    for expected in [
        "runnerd.slice",
        "halyard/ledger",
        "halyard/storefront",
        "Integration tests (Testcontainers)",
        "up to its MemoryMax of 33G",
        "peak",
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
        "sampled peak",
        "Waiting for a runner",
        "Set up job: line 1",
        "/live/jobs/",
        // Named as every client names a job, so the tab says which workflow's build it is.
        "<title>CI / build · runnerd</title>",
    ] {
        assert!(job.body.contains(expected), "job page lacks {expected:?}");
    }

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
async fn the_overviews_queue_lists_waiting_jobs_oldest_first_with_why_each_waits_once_each() {
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let mut older = fixtures.status.queued[0].clone();
        older.id -= 5;
        older.name = "lint".to_owned();
        older.url = Some("https://github.com/halyard/storefront/actions/runs/1/job/2".to_owned());
        older.queued_at -= 300;
        older.waiting = Some(Waiting::NoSlot {
            profile: "node".to_owned(),
        });
        fixtures.status.queued.insert(0, older);
        // The history keeps the newer one queued too.
        let mut recorded = fixtures.jobs[0].clone();
        recorded.summary.id = fixtures.status.queued[1].id;
        recorded.summary.state = JobState::Queued;
        fixtures.jobs.insert(0, recorded);
    }
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await;
    let body = &overview.body;
    let queue = &body[body.find(r#"id="queue""#).unwrap()..body.find(r#"id="budget""#).unwrap()];
    let lint = queue.find("https://github.com/halyard/storefront/actions/runs/1/job/2");
    let visual_diff = queue.find("/job/120055991710");
    assert!(
        lint.is_some() && lint < visual_diff,
        "the oldest queued job comes first: {queue}"
    );
    assert!(
        queue.contains("All 2 node slots are busy")
            && queue.contains("A runner is starting for it in node\u{a0}2"),
        "each queued job says why it waits: {queue}"
    );
    assert!(
        queue.contains(r#"<li data-class:linked="$_repo == 'halyard/storefront'""#),
        "a queued row lights up with its repository: {queue}"
    );
    assert_eq!(
        body.matches("120055991710").count(),
        1,
        "a queued job the history also has shows once, in the queue: {body}"
    );
    let health = &body[body.find(r#"id="health""#).unwrap()..body.find(r#"id="queue""#).unwrap()];
    assert!(
        health.contains(r##"<a class="figure-main" href="#queue">2</a>"##),
        "the health figures count both waiting: {health}"
    );
}

#[tokio::test]
async fn the_slices_charts_reach_up_to_the_slice_not_to_what_its_slots_add_up_to() {
    let dashboard = Dashboard::start().await;
    {
        // 32 threads, 33G; the slots' CPU limits add up past the CPUs, as quotas may.
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let profiles = &mut fixtures.status.profiles;
        for profile in profiles.iter_mut() {
            profile.cpus = 12;
        }
        let slots: usize = profiles.iter().map(|profile| profile.slots.len()).sum();
        assert!(
            slots * 12 > 32,
            "the slots ask for more CPUs than the slice has"
        );
    }
    let cookie = dashboard.sign_in().await;
    let body = dashboard.page(&cookie, "/").await.body;
    let host = &body[body.find(r#"id="host""#).unwrap()..];
    let axes: Vec<&str> = host
        .match_indices(r#"<div class="chart-y" aria-hidden="true"><span>"#)
        .map(|(at, prefix)| {
            let top = &host[at + prefix.len()..];
            &top[..top.find("</span>").unwrap()]
        })
        .collect();
    assert_eq!(
        axes,
        ["33G", "32 cores"],
        "MemoryMax and the slice's CPUs top the axes"
    );
    assert!(!host.contains("of all slots"), "{host}");
}

#[tokio::test]
async fn the_health_figures_count_the_24_hours_up_to_now_by_when_jobs_started_and_ended() {
    use runnerd_core::api::JobState;
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        // The dashboard's clock: the day ends now, whatever the fixtures say.
        let now = crate::now();
        let template = fixtures.jobs[0].clone();
        let job = |id, state, conclusion: &str, queued_at, started_at, finished_at| {
            let mut job = template.clone();
            job.summary.id = id;
            job.summary.state = state;
            job.summary.conclusion = Some(conclusion.to_owned());
            job.summary.timeline.queued_at = queued_at;
            job.summary.timeline.started_at = Some(started_at);
            job.summary.timeline.finished_at = Some(finished_at);
            job
        };
        let minutes = |count: u64| count * 60;
        fixtures.jobs = vec![
            // Ended 10 minutes ago after waiting 10.
            job(
                3,
                JobState::Completed,
                "success",
                now - minutes(40),
                now - minutes(30),
                now - minutes(10),
            ),
            // Queued three days before it started 23h 50m ago, lost 23h 45m ago: inside the day.
            job(
                2,
                JobState::Lost,
                "failure",
                now - 3 * 86_400 - minutes(23 * 60 + 50),
                now - minutes(23 * 60 + 50),
                now - minutes(23 * 60 + 45),
            ),
            // Failed 24h 15m ago: a day and a quarter hour ago, outside it.
            job(
                1,
                JobState::Completed,
                "failure",
                now - minutes(25 * 60),
                now - minutes(24 * 60 + 30),
                now - minutes(24 * 60 + 15),
            ),
        ];
    }
    let cookie = dashboard.sign_in().await;
    let body = dashboard.page(&cookie, "/").await.body;
    let health = &body[body.find(r#"id="health""#).unwrap()..body.find(r#"id="queue""#).unwrap()];
    let failed = &health[health.find("Failed, 24 h").unwrap()..];
    assert!(
        failed.contains(r#"<span class="figure-main">1</span>"#)
            && failed.contains("of 2 finished"),
        "one of the two jobs that ended in the day failed: {failed}"
    );
    assert!(
        health.contains("2 jobs started"),
        "the job queued days ago counts where it started: {health}"
    );
}

#[tokio::test]
async fn a_slot_card_counts_its_running_step_by_place_as_runnerctl_watch_does() {
    let dashboard = Dashboard::start().await;
    let (place, steps, name) = {
        let fixtures = dashboard.fake.fixtures.lock().unwrap();
        let job = fixtures
            .jobs
            .iter()
            .find(|job| job.summary.id == LEDGER_RUNNING)
            .unwrap();
        let place = job
            .steps
            .iter()
            .position(|step| step.status == "in_progress")
            .unwrap();
        (place + 1, job.steps.len(), job.steps[place].name.clone())
    };
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await.body;
    let expected = format!(r#"<span class="step-count">{place}/{steps}</span> {name}"#);
    assert!(
        overview.contains(&expected),
        "the card reads {expected}: {overview}"
    );
}

#[tokio::test]
async fn the_budget_names_the_mirror_and_each_seed_container() {
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let budget = &mut fixtures.status.budget;
        budget.seed_memory = vec![SeedMemory {
            profile: "jvm".to_owned(),
            memory: 9 << 30,
        }];
        budget.memory_in_use += 9 << 30;
    }
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await;
    for expected in [
        "<strong>512M</strong> held by the mirror",
        "<strong>9G</strong> held by jvm's seed container",
        r#"<span class="name">jvm seed</span>"#,
    ] {
        assert!(
            overview.body.contains(expected),
            "{expected}: {}",
            overview.body
        );
    }
}

#[tokio::test]
async fn the_budget_names_a_shell_open_in_a_record() {
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let budget = &mut fixtures.status.budget;
        budget.shell_memory = vec![ShellMemory {
            job: 42,
            memory: 9 << 30,
        }];
        budget.memory_in_use += 9 << 30;
    }
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await;
    assert!(
        overview
            .body
            .contains("<strong>9G</strong> held by the shell in job 42's record"),
        "the shell's memory is not shown as free: {}",
        overview.body
    );
}

/// The HTML from the start of the element whose `data-on:pointerenter` selects `key` to that attribute.
fn selector_of<'page>(page: &'page str, key: &str, tag: &str) -> &'page str {
    let at = page
        .find(&format!("data-on:pointerenter=\"$_slot = '{key}'\""))
        .unwrap_or_else(|| panic!("nothing selects {key}"));
    let start = page[..at].rfind(tag).unwrap();
    &page[start..at]
}

#[tokio::test]
async fn a_budget_block_opens_its_job_or_its_slots_card_and_lights_up_the_card() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await.body;
    let jvm_1 = slot_id("jvm", 1);
    let jvm_2 = slot_id("jvm", 2);
    assert!(
        selector_of(&overview, &jvm_1, "<a ")
            .contains(&format!(r#"href="/jobs/{LEDGER_RUNNING}""#)),
        "jvm 1's block opens the ledger build it runs"
    );
    assert!(
        selector_of(&overview, &jvm_2, "<a ").contains(&format!(r##"href="#{jvm_2}""##)),
        "the idle jvm 2's block points at its card"
    );
    for (profile, slot) in [("jvm", 1), ("jvm", 2), ("node", 1), ("node", 2)] {
        let card = slot_id(profile, slot);
        assert!(
            overview.contains(&format!(r#"<li id="{card}""#))
                && overview.contains(&format!("data-class:linked=\"$_slot == '{card}'")),
            "{profile} {slot}'s card exists and lights up with its block"
        );
    }
    // Every block's line is in the readout, toggled by a class: hiding one with `display: none` would let the
    // readout shrink and grow, and move the bar under the pointer.
    let lens = &overview[overview.find(r#"<p class="lens""#).unwrap()..];
    let lens = &lens[..lens.find("</p>").unwrap()];
    for (profile, slot) in [("jvm", 1), ("jvm", 2), ("node", 1), ("node", 2)] {
        assert!(
            lens.contains(&format!(
                "data-class:on=\"$_slot == '{}'\"",
                slot_id(profile, slot)
            )),
            "the readout has a line for {profile} {slot}"
        );
    }
    assert!(
        !lens.contains("display: none") && !lens.contains("data-show"),
        "the readout's lines keep their place while hidden"
    );
    // A block is named by what it shows and described by its line of the readout.
    for (key, line) in [
        (&jvm_1, "of 9G in jvm 1, build on halyard/ledger"),
        (&jvm_2, "9G jvm 2, idle"),
    ] {
        assert!(
            selector_of(&overview, key, "<a ")
                .contains(&format!(r#"aria-describedby="lens-{key}""#)),
            "{key}'s block is described by its line"
        );
        let described = &overview[overview.find(&format!(r#"<span id="lens-{key}""#)).unwrap()..];
        let text = described[..described.find("</span>").unwrap()]
            .replace("<strong>", "")
            .replace("</strong>", "");
        assert!(text.ends_with(line), "{key}'s line reads {line:?}");
    }
    assert!(
        overview.contains(r#"of 33G free""#),
        "the free remainder is named for screen readers"
    );
}

#[tokio::test]
async fn profiles_whose_names_differ_only_in_punctuation_keep_their_own_slots() {
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        fixtures.status.profiles[0].name = "node-js".to_owned();
        fixtures.status.profiles[1].name = "node_js".to_owned();
    }
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await.body;
    let dash = slot_id("node-js", 1);
    let underscore = slot_id("node_js", 1);
    assert_ne!(dash, underscore);
    for id in [&dash, &underscore] {
        assert_eq!(
            overview.matches(&format!(r#"id="{id}""#)).count(),
            1,
            "one card has the id {id}"
        );
        assert!(
            selector_of(&overview, id, "<a ").contains(&format!(r##"href="#{id}""##)),
            "the block for {id} points at its own card"
        );
    }
}

#[tokio::test]
async fn a_live_overview_lets_a_job_age_out_of_its_24_hours_with_no_job_event() {
    use runnerd_core::api::{HostSample, JobState};
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        // The dashboard's clock: a job that failed a day ago less two seconds, the only one of the day.
        let now = crate::now();
        let mut failed = fixtures.jobs[0].clone();
        failed.summary.state = JobState::Lost;
        failed.summary.timeline.queued_at = now - 86_400;
        failed.summary.timeline.started_at = Some(now - 86_400 + 1);
        failed.summary.timeline.finished_at = Some(now - 86_400 + 2);
        fixtures.jobs = vec![failed];
    }
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    let failed =
        |count: u32| format!(r#"Failed, 24 h</dt><dd><span class="figure-main">{count}</span>"#);
    read_until(&mut stream, |text| text.contains(&failed(1))).await;
    // Only host samples from here on, as on an idle evening.
    let events = dashboard.fake.events.clone();
    let samples = tokio::spawn(async move {
        loop {
            let at = crate::now();
            let _ = events.send(EventEnvelope {
                id: None,
                at,
                event: Event::HostSample {
                    sample: HostSample {
                        at,
                        cpu: 100,
                        memory: 1 << 30,
                    },
                },
            });
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    let expired = read_until(&mut stream, |text| text.contains(&failed(0))).await;
    samples.abort();
    assert!(
        expired.contains("0 jobs started"),
        "its wait aged out too: {expired}"
    );
}

#[tokio::test]
async fn the_selected_blocks_readout_follows_its_jobs_new_sample() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    read_until(&mut stream, |text| text.contains(r#"id="recent""#)).await;
    let at = NOW + 2;
    dashboard
        .fake
        .events
        .send(EventEnvelope {
            id: None,
            at,
            event: Event::JobSample {
                job: LEDGER_RUNNING,
                sample: Sample {
                    at,
                    cpu: 2000,
                    memory: 5 << 30,
                    network_received: 0,
                    network_sent: 0,
                },
            },
        })
        .unwrap();
    // The page keeps only which block is selected; its line comes in the patch, rendered from the new sample.
    let line = format!(
        "data-class:on=\"$_slot == '{}'\"><strong>5G</strong> of 9G in jvm 1",
        slot_id("jvm", 1)
    );
    read_until(&mut stream, |text| text.contains(&line)).await;
}

#[tokio::test]
async fn a_job_whose_samples_were_deleted_still_shows_its_memory_peak() {
    let dashboard = Dashboard::start().await;
    let peak = {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        fixtures.samples.insert(LEDGER_RUNNING, Vec::new());
        let job = fixtures
            .jobs
            .iter()
            .find(|job| job.summary.id == LEDGER_RUNNING)
            .unwrap();
        job.summary.metrics.memory_peak.unwrap()
    };
    let cookie = dashboard.sign_in().await;
    let job = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}"))
        .await
        .body;
    assert!(
        job.contains(&format!(
            "<dt>Memory peak over the job</dt><dd>{}</dd>",
            crate::format::bytes(peak)
        )),
        "the job's kept peak shows without samples"
    );
}

#[tokio::test]
async fn a_step_marks_its_own_stretch_of_the_memory_and_cpu_charts() {
    let dashboard = Dashboard::start().await;
    // The first step loses its samples, so it gets no band and the bands are not one per step.
    let (start, from, to) = {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let job = fixtures
            .jobs
            .iter()
            .find(|job| job.summary.id == LEDGER_RUNNING)
            .unwrap()
            .clone();
        let first = &job.steps[0];
        let (gone_from, gone_to) = (first.started_at.unwrap(), first.completed_at.unwrap());
        let samples = fixtures.samples.get_mut(&LEDGER_RUNNING).unwrap();
        samples.retain(|sample| !(gone_from..=gone_to).contains(&sample.at));
        let unit_tests = job
            .steps
            .iter()
            .find(|step| step.name == "Unit tests")
            .unwrap();
        (
            samples.first().unwrap().at,
            unit_tests.started_at.unwrap(),
            unit_tests.completed_at.unwrap(),
        )
    };
    let cookie = dashboard.sign_in().await;
    let job = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}"))
        .await
        .body;
    assert!(
        !job[..job.find(r#"<span class="step-name">Set up job"#).unwrap()]
            .ends_with(r#"data-class:linked="$_step == 1">"#),
        "the step without samples is not selectable"
    );
    let name = job.find(r#"<span class="step-name">Unit tests"#).unwrap();
    let focus = r#"data-on:focus="$_step = "#;
    let enter = job[..name].rfind(focus).unwrap() + focus.len();
    let number: usize = job[enter..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(number, 6, "Unit tests is the sixth step");
    // In seconds since the chart's start, its first sample.
    let stretch = format!(
        r#"data-class:on="$_step == {number}" style="--from: {}; --to: {}""#,
        from - start,
        to - start
    );
    assert_eq!(
        job.matches(&stretch).count(),
        2,
        "the step's band on the memory and on the CPU chart spans its own start and end"
    );
}

#[tokio::test]
async fn a_job_chart_shows_the_caps_its_container_ran_with_not_the_profiles_now() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    // jvm is configured with 9G and 8 CPUs now; the job's container ran with 12G and 6.
    dashboard.runner_caps(Some(12 << 30), Some(6000));
    let job = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}"))
        .await;
    for expected in ["slot cap 12G", "6 CPUs"] {
        assert!(job.body.contains(expected), "job page lacks {expected:?}");
    }
    assert!(
        !job.body.contains("jvm now"),
        "the job page shows today's config next to the job's own caps"
    );
}

#[tokio::test]
async fn a_job_recorded_without_its_caps_is_charted_against_the_profiles_limits_now() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    dashboard.runner_caps(None, None);
    let job = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}"))
        .await;
    for expected in ["jvm now 9G", "jvm now 8 CPUs", "limits in the config now"] {
        assert!(job.body.contains(expected), "job page lacks {expected:?}");
    }
    assert!(
        !job.body.contains("slot cap"),
        "the job page presents today's config as the job's caps"
    );
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
    let update = read_until(&mut stream, |text| text.contains(r#"id="host-cpu-now""#)).await;
    assert!(update.contains("31 cores"), "{update}");
}

/// The signals a page starts with, as the JSON Datastar sends them.
fn page_signals(page: &str) -> serde_json::Value {
    let from = page.find(r#"<body data-signals=""#).unwrap() + r#"<body data-signals=""#.len();
    let signals = &page[from..from + page[from..].find('"').unwrap()];
    // The page writes a JavaScript object.
    let json = signals
        .replace("{shown: {at:", r#"{"shown": {"at":"#)
        .replace(", log:", r#", "log":"#)
        .replace('\'', "\"");
    serde_json::from_str(&json).unwrap()
}

/// The `datastar` query parameter of a stream request carrying `signals`.
fn query(signals: &serde_json::Value) -> String {
    serde_urlencoded::to_string([("datastar", signals.to_string())]).unwrap()
}

/// The `datastar` query parameter a page's stream request carries.
fn signals_of(page: &str) -> String {
    query(&page_signals(page))
}

#[tokio::test]
async fn a_stream_opened_by_the_page_it_just_rendered_sends_none_of_its_regions_again() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let page = dashboard.page(&cookie, "/").await.body;
    let mut stream = dashboard
        .stream(&cookie, &format!("/live/overview?{}", signals_of(&page)))
        .await;
    let first = read_until(&mut stream, |text| text.contains("datastar-patch-signals")).await;
    assert!(
        !first.contains("<section"),
        "the page has every region already: {first}"
    );
    // A page that names no regions gets them all.
    let mut fresh = dashboard.stream(&cookie, "/live/overview").await;
    let first = read_until(&mut fresh, |text| text.contains("datastar-patch-signals")).await;
    assert_eq!(first.matches("<section").count(), 7, "{first}");
}

#[tokio::test]
async fn the_overview_shows_a_repository_resumed_from_another_client() {
    let dashboard = Dashboard::start().await;
    dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused = true;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    let first = read_until(&mut stream, |text| text.contains(r#"id="recent""#)).await;
    assert!(
        first.contains("badge neutral"),
        "halyard/ledger shows paused: {first}"
    );
    let origin = format!("http://{}", dashboard.addr);
    let reply = dashboard
        .request(
            "POST",
            "/v1/repositories/halyard/ledger/resume",
            &[("cookie", &cookie), ("origin", &origin)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let update = read_until(&mut stream, |text| text.contains(r#"id="repositories""#)).await;
    assert!(
        update.contains(&format!(r#"id="{}""#, repository_id("halyard/ledger")))
            && !update.contains("badge neutral"),
        "{update}"
    );
}

#[tokio::test]
async fn a_running_jobs_stream_appends_new_log_lines_and_updates_its_charts() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let lines = dashboard.fake.fixtures.lock().unwrap().logs[&LEDGER_RUNNING].len() as u64;
    let mut stream = dashboard
        .stream(&cookie, &format!("/live/jobs/{LEDGER_RUNNING}"))
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

#[tokio::test]
async fn a_job_stream_the_daemon_refuses_shows_why_and_ends_without_retrying() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/jobs/999").await;
    let mut text = String::new();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.frame().await {
                Some(Ok(frame)) => {
                    text.push_str(&String::from_utf8_lossy(
                        &frame.into_data().unwrap_or_default(),
                    ));
                }
                Some(Err(_)) => return "failed",
                None => return "ended",
            }
        }
    })
    .await
    .unwrap();
    assert!(text.contains("toast failed"), "{text}");
    // A retry would fail the same way and replace the notice.
    assert_eq!(end, "ended", "{text}");
}

#[tokio::test]
async fn a_job_stream_that_misses_log_lines_fails_so_the_page_connects_again() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let lines = dashboard.fake.fixtures.lock().unwrap().logs[&LEDGER_RUNNING].len() as u64;
    let mut stream = dashboard
        .stream(&cookie, &format!("/live/jobs/{LEDGER_RUNNING}"))
        .await;
    read_until(&mut stream, |text| text.contains(r#"id="metrics""#)).await;
    dashboard
        .fake
        .events
        .send(EventEnvelope {
            id: None,
            at: NOW + 2,
            event: Event::LogLine {
                job: LEDGER_RUNNING,
                line: lines + 1,
                text: "BUILD SUCCESSFUL in 2m 28s".to_owned(),
            },
        })
        .unwrap();
    // Datastar retries a stream that fails, not one that ends.
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.frame().await {
                Some(Ok(frame)) => {
                    let data = frame.into_data().unwrap_or_default();
                    assert!(
                        !String::from_utf8_lossy(&data).contains("BUILD SUCCESSFUL"),
                        "a line after a gap is not appended"
                    );
                }
                Some(Err(_)) => return "failed",
                None => return "ended",
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(end, "failed");
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
                    .map(|(_, value)| value.split('"').next().unwrap().parse().unwrap());
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
async fn a_job_stream_that_reconnects_shows_each_log_line_once() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let (page, url) = dashboard.job_page(&cookie).await;
    let mut shown = ShownLog::of_page(&page);
    let mut first = dashboard.stream(&cookie, &url).await;
    read_until(&mut first, |text| text.contains(r#"id="metrics""#)).await;
    for n in 0..5 {
        dashboard.log(&format!("before the drop {n}"));
    }
    shown.apply(&read_until(&mut first, |text| text.contains("before the drop 4")).await);
    drop(first);
    // Datastar retries the URL the page named.
    dashboard.log("while away");
    let mut second = dashboard.stream(&cookie, &url).await;
    shown.apply(&read_until(&mut second, |text| text.contains(r#"id="metrics""#)).await);
    let texts: Vec<&str> = shown.0.iter().map(|line| line.1.as_str()).collect();
    for n in 0..5 {
        let text = format!("before the drop {n}");
        assert_eq!(
            texts.iter().filter(|shown| **shown == text).count(),
            1,
            "{texts:?}"
        );
    }
    assert_eq!(texts.last(), Some(&"while away"));
    let mut numbers: Vec<Option<u64>> = shown.0.iter().map(|line| line.0).collect();
    numbers.dedup();
    assert_eq!(
        numbers.len(),
        shown.0.len(),
        "each line has its own number: {texts:?}"
    );
}

#[tokio::test]
async fn a_job_stream_that_reconnects_with_the_pages_signals_sends_only_the_lines_after_its_last() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let (page, url) = dashboard.job_page(&cookie).await;
    let mut shown = ShownLog::of_page(&page);
    let signals = signals_of(&page);
    let mut first = dashboard.stream(&cookie, &format!("{url}?{signals}")).await;
    let connected = read_until(&mut first, |text| text.contains("datastar-patch-signals")).await;
    assert!(
        !connected.contains(r#"id="log-lines""#),
        "the page has the tail already: {connected}"
    );
    for n in 0..5 {
        dashboard.log(&format!("before the drop {n}"));
    }
    let lines = read_until(&mut first, |text| text.contains("before the drop 4")).await;
    shown.apply(&lines);
    drop(first);
    // The last line appended set the page's `shown.log`, which Datastar sends when it retries.
    let last = lines.rsplit(r#"data-signals:shown.log=""#).next().unwrap();
    let last: u64 = last[..last.find('"').unwrap()].parse().unwrap();
    let mut retried = page_signals(&page);
    retried["shown"]["log"] = last.into();
    dashboard.log("while away");
    let mut second = dashboard
        .stream(&cookie, &format!("{url}?{}", query(&retried)))
        .await;
    let again = read_until(&mut second, |text| text.contains("while away")).await;
    assert!(!again.contains(r#"id="log-lines""#), "{again}");
    shown.apply(&again);
    let texts: Vec<&str> = shown.0.iter().map(|line| line.1.as_str()).collect();
    for n in 0..5 {
        let text = format!("before the drop {n}");
        assert_eq!(
            texts.iter().filter(|shown| **shown == text).count(),
            1,
            "{texts:?}"
        );
    }
    assert_eq!(texts.last(), Some(&"while away"));
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
    // 2 × 12G + 2 × 7G + 512M = 38.5G, over runnerd.slice's 33G.
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
        "node 2 7G",
        "MemoryHigh=30G",
    ] {
        assert!(
            page.body.contains(expected),
            "config page lacks {expected:?}"
        );
    }
}

#[tokio::test]
async fn an_auto_sizing_change_is_listed_and_undoing_it_opens_what_it_replaced_in_the_editor() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let sizing = dashboard.page(&cookie, "/sizing").await;
    for expected in ["memory 10G → 9G, slots 2", r#"href="/config?undo=jvm""#] {
        assert!(
            sizing.body.contains(expected),
            "sizing page lacks {expected:?}"
        );
    }
    let before = dashboard.fake.fixtures.lock().unwrap().config.clone();
    let editor = dashboard.page(&cookie, "/config?undo=jvm").await;
    assert_eq!(editor.status, StatusCode::OK);
    assert!(
        editor.body.contains("memory = &quot;10G&quot;\nslots = 2"),
        "the editor holds jvm's memory from before the change"
    );
    assert_eq!(
        dashboard.fake.fixtures.lock().unwrap().config,
        before,
        "opening the editor writes nothing"
    );
}

#[tokio::test]
async fn after_a_save_the_editor_holds_the_revision_of_what_it_saved_not_of_a_config_written_since()
{
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let before = dashboard.fake.fixtures.lock().unwrap().config.clone();
    let saved_text = before.replacen("slots = 2", "slots = 1", 1);
    // Auto sizing writes C between the save and any read after it.
    let written_since = before.replacen("memory = \"7G\"", "memory = \"5.25G\"", 1);
    dashboard.fake.fixtures.lock().unwrap().after_put = Some(written_since.clone());
    let saved = dashboard
        .write(
            &cookie,
            "PUT",
            "/config",
            &serde_json::json!({ "toml": saved_text, "revision": runnerd_core::config::revision(&before) }),
        )
        .await;
    assert!(saved.body.contains("Saved and reloaded"), "{}", saved.body);
    assert!(
        saved.body.contains(&format!(
            "'{}'",
            runnerd_core::config::revision(&saved_text)
        )),
        "the editor saves next against the text it saved"
    );
    assert!(
        !saved
            .body
            .contains(&runnerd_core::config::revision(&written_since)),
        "the editor never takes the revision of a text it does not hold"
    );
}

#[tokio::test]
async fn an_editor_opened_before_the_config_changed_cannot_save_over_the_change() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let before = dashboard.fake.fixtures.lock().unwrap().config.clone();
    let editor = dashboard.page(&cookie, "/config").await;
    let read = runnerd_core::config::revision(&before);
    assert!(
        editor
            .body
            .contains(&format!("data-signals:revision=\"'{read}'\"")),
        "the editor keeps the revision it was opened with"
    );
    // As when auto sizing lowers node while the editor is open.
    let changed = before.replacen("memory = \"7G\"", "memory = \"5.25G\"", 1);
    dashboard.fake.fixtures.lock().unwrap().config = changed.clone();
    let stale = before.replacen("slots = 2", "slots = 1", 1);
    let saved = dashboard
        .write(
            &cookie,
            "PUT",
            "/config",
            &serde_json::json!({ "toml": stale, "revision": read }),
        )
        .await;
    assert!(saved.body.contains("Not saved"), "{}", saved.body);
    assert_eq!(
        dashboard.fake.fixtures.lock().unwrap().config,
        changed,
        "the change made meanwhile stays"
    );
    let fresh = runnerd_core::config::revision(&changed);
    let saved = dashboard
        .write(
            &cookie,
            "PUT",
            "/config",
            &serde_json::json!({ "toml": changed.replacen("slots = 2", "slots = 1", 1), "revision": fresh }),
        )
        .await;
    assert!(saved.body.contains("Saved and reloaded"), "{}", saved.body);
    let next = runnerd_core::config::revision(&dashboard.fake.fixtures.lock().unwrap().config);
    assert!(
        saved.body.contains(&format!("'{next}'")),
        "the next save is made against what was saved"
    );
}

#[tokio::test]
async fn sizing_advice_opens_in_the_config_editor_and_is_written_only_when_saved() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let sizing = dashboard.page(&cookie, "/sizing").await;
    assert_eq!(sizing.status, StatusCode::OK);
    for expected in [
        "lower memory to 2.5G from 7G, its largest peak + 15%; 5 slots fit in the same 14G",
        "keep 9G: its largest peak + 15% is 9G",
        r#"href="/config?resize=node""#,
        "Review memory 2.5G, 5 slots in the config",
        "integration tests",
        "halyard/storefront · CI",
        "waits for 10 jobs",
    ] {
        assert!(
            sizing.body.contains(expected),
            "sizing page lacks {expected:?}"
        );
    }
    assert!(
        !sizing.body.contains("/config?resize=jvm"),
        "jvm has nothing to apply"
    );

    let before = dashboard.fake.fixtures.lock().unwrap().config.clone();
    let editor = dashboard.page(&cookie, "/config?resize=node").await;
    assert_eq!(editor.status, StatusCode::OK);
    let advised = before.replace("memory = \"7G\"\nslots = 2", "memory = \"2.5G\"\nslots = 5");
    assert_ne!(advised, before);
    for expected in [
        "memory = &quot;2.5G&quot;\nslots = 5",
        "Nothing is written until you save it.",
        // 2 × 9G + 5 × 2.5G + 512M.
        "31G",
    ] {
        assert!(editor.body.contains(expected), "editor lacks {expected:?}");
    }
    assert_eq!(
        dashboard.fake.fixtures.lock().unwrap().config,
        before,
        "opening the editor writes nothing"
    );
    let saved = dashboard
        .write(
            &cookie,
            "PUT",
            "/config",
            &serde_json::json!({ "toml": advised }),
        )
        .await;
    assert!(saved.body.contains("Saved and reloaded"), "{}", saved.body);
    assert_eq!(dashboard.fake.fixtures.lock().unwrap().config, advised);
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
        asked.body.contains(">Keep it running<"),
        "the question's dismissal speaks of the job: {}",
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
        forced
            .body
            .contains(&format!(r#"id="{}""#, repository_id("halyard/ledger")))
            && forced.body.contains("stopped job"),
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

#[tokio::test]
async fn a_failed_jobs_record_shows_its_shell_command_and_goes_from_the_caches_page() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let (failed, others) = {
        let fixtures = dashboard.fake.fixtures.lock().unwrap();
        let jobs: Vec<u64> = fixtures.records.records.iter().map(|r| r.job).collect();
        (jobs[0], jobs.len() - 1)
    };
    let job = dashboard
        .page(&cookie, &format!("/jobs/{failed}"))
        .await
        .body;
    assert!(
        job.contains("Open a shell") && job.contains(&format!("runnerctl shell {failed}")),
        "{job}"
    );
    let caches = dashboard.page(&cookie, "/caches").await.body;
    assert!(
        caches.contains(&format!("@delete('/v1/records/{failed}')")),
        "{caches}"
    );

    let removed = dashboard
        .write(
            &cookie,
            "DELETE",
            &format!("/v1/records/{failed}"),
            &serde_json::json!({}),
        )
        .await;
    assert!(
        removed.body.contains("Removed 1 record"),
        "{}",
        removed.body
    );
    assert!(
        !removed.body.contains(&format!("/v1/records/{failed}'")),
        "the section shows the record gone: {}",
        removed.body
    );
    let job = dashboard
        .page(&cookie, &format!("/jobs/{failed}"))
        .await
        .body;
    assert!(
        !job.contains("runnerctl shell") && job.contains("removed by hand"),
        "{job}"
    );

    let all = dashboard
        .write(
            &cookie,
            "DELETE",
            "/v1/records?all=true",
            &serde_json::json!({}),
        )
        .await;
    assert!(
        all.body.contains(&format!("Removed {others} records")),
        "{}",
        all.body
    );
    assert!(all.body.contains("None kept"), "{}", all.body);
}

#[tokio::test]
async fn the_overview_offers_pause_all_until_every_repository_is_paused_then_resume_all() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let pause_all = "@post('/v1/repositories/pause')";
    let resume_all = "@post('/v1/repositories/resume')";
    let none = dashboard.page(&cookie, "/").await.body;
    assert!(
        none.contains(pause_all) && !none.contains(resume_all),
        "nothing paused offers only pause all"
    );
    dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused = true;
    let some = dashboard.page(&cookie, "/").await.body;
    assert!(
        some.contains(pause_all) && some.contains(resume_all),
        "one of two paused offers both"
    );
    dashboard.fake.fixtures.lock().unwrap().status.repositories[1].paused = true;
    let all = dashboard.page(&cookie, "/").await.body;
    assert!(
        !all.contains(pause_all) && all.contains(resume_all),
        "everything paused offers only resume all"
    );
    let resumed = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/repositories/resume",
            &serde_json::json!({}),
        )
        .await;
    assert!(
        resumed.body.contains(r#"id="repositories""#) && resumed.body.contains(pause_all),
        "resume all returns the rows, offering pause all again: {}",
        resumed.body
    );
    let fixtures = dashboard.fake.fixtures.lock().unwrap();
    assert!(
        fixtures
            .status
            .repositories
            .iter()
            .all(|repository| !repository.paused),
        "every repository runs again"
    );
    assert_eq!(fixtures.controls, ["resume all force=false"]);
}

#[tokio::test]
async fn every_buttons_name_starts_with_the_label_it_shows() {
    let dashboard = Dashboard::start().await;
    // One repository paused, so the overview offers both pause all and resume all.
    dashboard.fake.fixtures.lock().unwrap().status.repositories[0].paused = true;
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await.body;
    let buttons: Vec<&str> = overview.split("<button").skip(1).collect();
    assert!(buttons.len() > 3, "{} buttons", buttons.len());
    for button in buttons {
        let (tag, rest) = button.split_once('>').unwrap();
        let Some(name) = tag.split(r#"aria-label=""#).nth(1) else {
            continue;
        };
        let name = &name[..name.find('"').unwrap()];
        let label: String = rest[..rest.find("</button>").unwrap()]
            .split('<')
            .map(|text| text.split_once('>').map_or(text, |(_, text)| text))
            .collect();
        if !label.trim().is_empty() {
            assert!(
                name.starts_with(label.trim()),
                "a button showing {label:?} is named {name:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_single_repository_has_only_its_own_pause() {
    let dashboard = Dashboard::start().await;
    dashboard
        .fake
        .fixtures
        .lock()
        .unwrap()
        .status
        .repositories
        .truncate(1);
    let cookie = dashboard.sign_in().await;
    let overview = dashboard.page(&cookie, "/").await.body;
    assert!(
        !overview.contains("@post('/v1/repositories/pause')"),
        "no pause all beside the one pause"
    );
}

#[tokio::test]
async fn pausing_every_repository_while_jobs_run_asks_first_naming_them_and_cancels_only_when_told()
{
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let storefront_running = dashboard
        .fake
        .fixtures
        .lock()
        .unwrap()
        .jobs
        .iter()
        .find(|job| {
            job.summary.state == JobState::Running && job.summary.repository == "halyard/storefront"
        })
        .unwrap()
        .summary
        .id;
    let asked = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/repositories/pause",
            &serde_json::json!({}),
        )
        .await;
    for job in [LEDGER_RUNNING, storefront_running] {
        assert!(
            asked.body.contains(&job.to_string()),
            "the question names job {job}: {}",
            asked.body
        );
    }
    assert!(
        asked
            .body
            .contains("@post('/v1/repositories/pause?force=true')")
            && !asked.body.contains("pause with force"),
        "the question offers the forced pause of every repository: {}",
        asked.body
    );
    assert!(
        asked.body.contains(">Keep them running<"),
        "the question's dismissal speaks of the jobs: {}",
        asked.body
    );
    assert!(
        !dashboard
            .fake
            .fixtures
            .lock()
            .unwrap()
            .status
            .repositories
            .iter()
            .any(|repository| repository.paused),
        "a refused pause pauses none"
    );
    let forced = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/repositories/pause?force=true",
            &serde_json::json!({}),
        )
        .await;
    assert!(
        forced.body.contains(r#"id="repositories""#)
            && forced.body.contains("@post('/v1/repositories/resume')")
            && forced.body.contains("stopped job"),
        "{}",
        forced.body
    );
    let fixtures = dashboard.fake.fixtures.lock().unwrap();
    assert!(
        fixtures
            .status
            .repositories
            .iter()
            .all(|repository| repository.paused),
        "every repository is paused"
    );
    assert_eq!(
        fixtures.controls,
        ["pause all force=false", "pause all force=true"]
    );
}

#[tokio::test]
async fn resuming_every_repository_when_the_history_cannot_record_it_says_so() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    dashboard.fake.fixtures.lock().unwrap().unrecorded = true;
    let reply = dashboard
        .write(
            &cookie,
            "POST",
            "/v1/repositories/resume",
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.contains(r#"id="notice""#)
            && reply.body.contains("toast failed")
            && reply
                .body
                .contains("cannot record it, so a restart pauses it again"),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_jobs_page_compares_it_with_the_same_jobs_latest_runs_only() {
    let dashboard = Dashboard::start().await;
    let (this, same, others) = {
        let fixtures = dashboard.fake.fixtures.lock().unwrap();
        let this = fixtures.jobs[0].summary.clone();
        let same_job = |job: &&runnerd_core::api::Job| {
            job.summary.repository == this.repository
                && job.summary.workflow == this.workflow
                && job.summary.name == this.name
        };
        let same: Vec<u64> = fixtures
            .jobs
            .iter()
            .filter(same_job)
            .map(|job| job.summary.id)
            .collect();
        let others: Vec<u64> = fixtures
            .jobs
            .iter()
            .filter(|job| !same_job(job))
            .map(|job| job.summary.id)
            .collect();
        (this, same, others)
    };
    assert!(
        same.len() > 1 && !others.is_empty(),
        "the fixtures have both"
    );
    let cookie = dashboard.sign_in().await;
    let page = dashboard
        .page(&cookie, &format!("/jobs/{}", this.id))
        .await
        .body;
    let runs = &page[page.find(r#"id="runs""#).unwrap()..];
    let runs = &runs[..runs.find("</section>").unwrap()];
    for id in same.iter().take(30) {
        assert!(
            runs.contains(&format!(r#"href="/jobs/{id}""#)),
            "run {id} is compared: {runs}"
        );
    }
    for id in &others {
        assert!(
            !runs.contains(&format!("/jobs/{id}\"")),
            "job {id} is another job: {runs}"
        );
    }
    assert!(
        runs.contains(&format!(r#"href="/jobs/{}" aria-label="#, this.id))
            && runs.contains(r#"aria-current="page""#),
        "this run is marked among them: {runs}"
    );
}

#[tokio::test]
async fn history_filters_are_links_that_keep_the_other_filters() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let page = dashboard
        .page(&cookie, "/history?outcome=success")
        .await
        .body;
    let filters = &page[page.find(r#"class="filters""#).unwrap()..];
    let filters = &filters[..filters.find("</nav>").unwrap()];
    assert!(
        filters.contains(r#"<a href="/history?outcome=success&amp;workflow=E2E">E2E</a>"#),
        "a workflow's link keeps the outcome: {filters}"
    );
    assert!(
        filters.contains(r#"<a href="/history?outcome=success" aria-current="true">Succeeded</a>"#),
        "the outcome in force is marked: {filters}"
    );
    let e2e = dashboard.page(&cookie, "/history?workflow=E2E").await.body;
    let rows = &e2e[e2e.find("<tbody>").unwrap()..];
    assert!(
        rows.contains("playwright") && !rows.contains(">build<"),
        "only E2E's jobs are listed: {rows}"
    );
}

#[tokio::test]
async fn the_sizing_page_says_what_auto_sizing_does_next_with_each_profile() {
    use runnerd_core::api::{AutoPlan, Resize, SizingChange, SizingHold};
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let off = dashboard.page(&cookie, "/sizing").await.body;
    assert!(off.contains("Auto sizing is off"), "{off}");
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let now = fixtures.now;
        fixtures.auto_plan = Some(AutoPlan {
            every: 300,
            interval: 86_400,
            max_step: 25,
            next: vec![SizingChange {
                at: now,
                profile: "node".to_owned(),
                from: Resize {
                    memory: 7 << 30,
                    slots: 2,
                },
                to: Resize {
                    memory: 5376 << 20,
                    slots: 2,
                },
                reason: "lower memory".to_owned(),
                uncertain: false,
            }],
            held: vec![SizingHold {
                profile: "jvm".to_owned(),
                changed: now - 3600,
                until: now + 23 * 3600,
            }],
            blocked: None,
        });
    }
    let on = dashboard.page(&cookie, "/sizing").await.body;
    let plan = &on[on.find(r#"class="plan-list""#).unwrap()..];
    let plan = &plan[..plan.find("</ul>").unwrap()];
    let line = |profile: &str| {
        let from = plan.find(&format!("</span>{profile}</span>")).unwrap();
        plan[from..].split("</li>").next().unwrap().to_owned()
    };
    assert!(
        line("node").contains("Next look, within 5 minutes: memory 7G → 5.25G, slots 2"),
        "node changes at the next look: {plan}"
    );
    assert!(
        line("jvm").contains("No change before") && !line("jvm").contains("Next look"),
        "jvm is held: {plan}"
    );
    assert!(
        on.contains("at most once every 24 hours"),
        "the rules in words: {on}"
    );
    // The file was edited by hand: no look writes the change, so the page says why instead of promising it.
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let plan = fixtures.auto_plan.as_mut().unwrap();
        plan.blocked = Some("the file changed on disk since runnerd read it".to_owned());
    }
    let blocked = dashboard.page(&cookie, "/sizing").await.body;
    assert!(
        blocked.contains("No look changes anything now: the file changed on disk"),
        "the blocker shows: {blocked}"
    );
    assert!(
        !blocked.contains("Next look") && blocked.contains("Once that is fixed: memory 7G"),
        "the change is not promised for the next look: {blocked}"
    );
}
