//! Who may use the dashboard. The port is open to every local user, so every request but the login and the static
//! assets needs the session cookie, and every write also needs an `Origin` of the dashboard itself: its loopback
//! address, or one of `[web] origins`, the addresses of a reverse proxy in front of it.
//!
//! The cookie holds the session secret (`Sessions`), which the daemon keeps in a file only its user can read, so a
//! signed-in browser stays signed in across restarts until the secret is replaced (`Sessions::sign_out_all`). A
//! browser gets the cookie by opening a one-time login link: one per address in `login_file`, which only the
//! daemon's user can read and which gets a new link in place of each one used, or one a signed-in page asks for to
//! show as a QR code (`pair`), which also expires after `PAIRING_LIFETIME`. This module owns the secret, the codes
//! and both files; it never logs any of them.

use std::fmt::{self, Write as _};
use std::io::{self, Read as _, Write as _};
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use runnerd_core::config::Origin;
use serde::Deserialize;
use tracing::warn;

use crate::views::{self, SignedOut};

/// How long a signed-in browser stays signed in without using the dashboard. Each authenticated response sets the
/// cookie again, so a phone opened once a month never signs out; one lost in a drawer does after this.
const SESSION_LIFETIME: Duration = Duration::from_hours(30 * 24);

/// How long a code shown as a QR code works: enough to pick up a phone and scan it.
pub(crate) const PAIRING_LIFETIME: Duration = Duration::from_mins(5);

/// The most pairing codes alive at once; asking for another drops the oldest.
const PAIRING_MAX: usize = 8;

/// Where the daemon serving its API on `socket` keeps the dashboard's login links, one per line: its loopback
/// address first, then each of `[web] origins`. `runnerd.sock` → `runnerd.web`. `runnerctl web` reads it.
pub fn login_file(socket: &Path) -> PathBuf {
    socket.with_extension("web")
}

/// Where the daemon keeps the session secret: beside its job history, `database`.
pub fn session_file(database: &Path) -> PathBuf {
    database.with_file_name("dashboard-session")
}

/// 32 random bytes as hex.
fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

/// Compares in time independent of where the strings differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

/// Writes `text` to `file`, readable by this user only, replacing it atomically.
fn write_private(file: &Path, text: &str) -> io::Result<()> {
    let mut temporary = file.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    let _ = std::fs::remove_file(&temporary);
    let mut written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    written.write_all(text.as_bytes())?;
    written.sync_all()?;
    std::fs::rename(&temporary, file)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The dashboard's session secret, kept in a file only the daemon's user can read. Every browser signed in holds it
/// in its cookie; replacing it signs them all out.
pub struct Sessions {
    file: PathBuf,
    secret: Mutex<String>,
}

impl fmt::Debug for Sessions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sessions")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl Sessions {
    /// The secret in `file`, or a new one written there when it is missing, malformed, or readable by other users.
    pub fn open(file: PathBuf) -> io::Result<Arc<Sessions>> {
        let kept = match std::fs::read_to_string(&file) {
            Ok(text) => {
                let others = std::fs::metadata(&file)?.permissions().mode() & 0o077;
                let private = others == 0;
                let secret = text.trim();
                if !private {
                    warn!(
                        "{} is readable by other users; signing every browser out",
                        file.display()
                    );
                }
                (private && secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()))
                    .then(|| secret.to_owned())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let secret = if let Some(secret) = kept {
            secret
        } else {
            let secret = random_token()?;
            write_private(&file, &format!("{secret}\n"))?;
            secret
        };
        Ok(Arc::new(Sessions {
            file,
            secret: Mutex::new(secret),
        }))
    }

    /// Replaces the secret, so every browser signed in is signed out.
    pub fn sign_out_all(&self) -> io::Result<()> {
        let mut secret = lock(&self.secret);
        let next = random_token()?;
        write_private(&self.file, &format!("{next}\n"))?;
        *secret = next;
        Ok(())
    }

    fn secret(&self) -> String {
        lock(&self.secret).clone()
    }

    fn is(&self, value: &str) -> bool {
        same(value, &lock(&self.secret))
    }
}

/// A code shown as a QR code, until it expires.
struct Pairing {
    code: String,
    expires: Instant,
}

/// The login codes alive: one per link in the login file, in its order, and those asked for to pair a device.
struct Codes {
    links: Vec<String>,
    pairings: Vec<Pairing>,
}

/// The session, the login codes and where their links are written.
pub(crate) struct Auth {
    sessions: Arc<Sessions>,
    codes: Mutex<Codes>,
    file: PathBuf,
    cookie: String,
    /// The origins whose pages may write: the loopback address, `localhost` on its port, and the proxies.
    origins: Vec<String>,
    /// The origins the login file has a link for: the loopback address, then the proxies.
    bases: Vec<String>,
    proxies: Vec<String>,
}

impl fmt::Debug for Auth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Auth")
            .field("sessions", &self.sessions)
            .field("file", &self.file)
            .field("cookie", &self.cookie)
            .field("origins", &self.origins)
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// The session for a dashboard on `addr`, which also takes writes from `proxies`; writes a login link for each
    /// to `file`.
    pub(crate) fn new(
        addr: SocketAddr,
        proxies: &[Origin],
        sessions: Arc<Sessions>,
        file: PathBuf,
    ) -> io::Result<Arc<Auth>> {
        let base = format!("http://{addr}");
        let proxies: Vec<String> = proxies
            .iter()
            .map(|origin| origin.as_str().to_owned())
            .collect();
        let mut origins = vec![base.clone()];
        if addr.ip().is_loopback() {
            origins.push(format!("http://localhost:{}", addr.port()));
        }
        origins.extend(proxies.iter().cloned());
        let bases: Vec<String> = std::iter::once(base)
            .chain(proxies.iter().cloned())
            .collect();
        let links = bases
            .iter()
            .map(|_| random_token())
            .collect::<io::Result<_>>()?;
        let auth = Auth {
            sessions,
            codes: Mutex::new(Codes {
                links,
                pairings: Vec::new(),
            }),
            file,
            cookie: format!("runnerd-{}", addr.port()),
            origins,
            bases,
            proxies,
        };
        auth.write_login_file(&lock(&auth.codes).links)?;
        Ok(Arc::new(auth))
    }

    /// `[web] origins`, the addresses a QR code can sign a phone in on.
    pub(crate) fn proxies(&self) -> &[String] {
        &self.proxies
    }

    /// Writes a login link per base with its code in `links`.
    fn write_login_file(&self, links: &[String]) -> io::Result<()> {
        let text = self
            .bases
            .iter()
            .zip(links)
            .fold(String::new(), |mut text, (base, code)| {
                let _ = writeln!(text, "{base}/login?code={code}");
                text
            });
        write_private(&self.file, &text)
    }

    /// Takes `code` if it is alive at `now`. A link's code is replaced by a new one in the login file.
    fn redeem(&self, code: &str, now: Instant) -> bool {
        let mut codes = lock(&self.codes);
        codes.pairings.retain(|pairing| pairing.expires > now);
        if let Some(index) = codes
            .pairings
            .iter()
            .position(|pairing| same(code, &pairing.code))
        {
            codes.pairings.remove(index);
            return true;
        }
        let Some(index) = codes.links.iter().position(|link| same(code, link)) else {
            return false;
        };
        match random_token() {
            Ok(next) => {
                codes.links[index] = next;
                if let Err(error) = self.write_login_file(&codes.links) {
                    warn!(
                        "cannot write the dashboard's login links to {}: {error}",
                        self.file.display()
                    );
                }
            }
            // The used code stays out: a code works once even when no new one can be made.
            Err(error) => {
                codes.links[index] = String::new();
                warn!("cannot make a new dashboard login code: {error}");
            }
        }
        true
    }

    /// A login link on `origin`, one of the proxies, that works once until `now` + `PAIRING_LIFETIME`.
    fn pair(&self, origin: &str, now: Instant) -> io::Result<String> {
        if !self.proxies.iter().any(|proxy| proxy == origin) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{origin} is not one of [web] origins"),
            ));
        }
        let code = random_token()?;
        let mut codes = lock(&self.codes);
        codes.pairings.retain(|pairing| pairing.expires > now);
        if codes.pairings.len() >= PAIRING_MAX {
            codes.pairings.remove(0);
        }
        codes.pairings.push(Pairing {
            code: code.clone(),
            expires: now + PAIRING_LIFETIME,
        });
        Ok(format!("{origin}/login?code={code}"))
    }

    /// Removes the login file, when the dashboard stops.
    pub(crate) fn remove_login_file(&self) {
        let _ = std::fs::remove_file(&self.file);
    }

    fn has_session(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(name, value)| name == self.cookie && self.sessions.is(value))
    }

    fn same_origin(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::ORIGIN)
            .and_then(|origin| origin.to_str().ok())
            .is_some_and(|origin| self.origins.iter().any(|allowed| allowed == origin))
    }

    /// The session cookie. Through a proxy that says it took the request over https (`X-Forwarded-Proto`, as
    /// `tailscale serve` sends), it is `Secure`.
    fn cookie(&self, headers: &HeaderMap) -> Option<HeaderValue> {
        let https = headers
            .get("x-forwarded-proto")
            .is_some_and(|proto| proto.as_bytes().eq_ignore_ascii_case(b"https"));
        HeaderValue::try_from(format!(
            "{}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{}",
            self.cookie,
            self.sessions.secret(),
            SESSION_LIFETIME.as_secs(),
            if https { "; Secure" } else { "" }
        ))
        .ok()
    }
}

#[derive(Deserialize)]
pub(crate) struct Login {
    code: String,
}

/// `GET /login?code=…`: a live code sets the session cookie and goes to the overview.
pub(crate) async fn login(
    State(auth): State<Arc<Auth>>,
    Query(login): Query<Login>,
    headers: HeaderMap,
) -> Response {
    if !auth.redeem(&login.code, Instant::now()) {
        return (
            StatusCode::UNAUTHORIZED,
            views::signed_out(SignedOut::CodeUsed),
        )
            .into_response();
    }
    let mut response = Redirect::to("/").into_response();
    if let Some(cookie) = auth.cookie(&headers) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

#[derive(Deserialize)]
pub(crate) struct PairRequest {
    /// The proxy to sign in on; the first when the page sends none.
    #[serde(default)]
    pairing: String,
}

/// `POST /pair`: a fresh one-time login link on one of `[web] origins`, as a QR code the page shows.
pub(crate) async fn pair(
    State(auth): State<Arc<Auth>>,
    request: Option<axum::Json<PairRequest>>,
) -> Response {
    let wanted = request.map(|request| request.0.pairing).unwrap_or_default();
    let origin = if wanted.is_empty() {
        auth.proxies().first().cloned().unwrap_or_default()
    } else {
        wanted
    };
    let markup = match auth.pair(&origin, Instant::now()) {
        Ok(url) => views::config::pairing_code(&url, &origin),
        Err(error) => views::notice(&error.to_string(), views::Tone::Failed),
    };
    axum::response::Html(markup.into_string()).into_response()
}

/// Lets through the login, the assets, and requests with the session cookie, which they get again with its
/// lifetime renewed; writes also need the dashboard's own `Origin` or a proxy's. A missing or wrong cookie is 401,
/// a foreign origin 403.
pub(crate) async fn require_session(
    State(auth): State<Arc<Auth>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path == "/login" || path.starts_with("/assets/") {
        return next.run(request).await;
    }
    if !auth.has_session(request.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            views::signed_out(SignedOut::NoSession),
        )
            .into_response();
    }
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && !auth.same_origin(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "writes must come from the dashboard's own pages",
        )
            .into_response();
    }
    let cookie = auth.cookie(request.headers());
    let mut response = next.run(request).await;
    if let Some(cookie) = cookie {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(file: &Path) -> Vec<String> {
        std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn code(link: &str) -> &str {
        link.split_once("code=").unwrap().1
    }

    fn mode(file: &Path) -> u32 {
        std::fs::metadata(file).unwrap().permissions().mode() & 0o777
    }

    fn dashboard(dir: &Path, proxies: &[&str]) -> Arc<Auth> {
        let proxies: Vec<Origin> = proxies.iter().map(|p| p.parse().unwrap()).collect();
        let sessions = Sessions::open(dir.join("dashboard-session")).unwrap();
        Auth::new(
            "127.0.0.1:7477".parse().unwrap(),
            &proxies,
            sessions,
            dir.join("runnerd.web"),
        )
        .unwrap()
    }

    #[test]
    fn a_login_code_works_once_and_the_file_then_holds_a_new_one() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[]);
        let file = dir.0.join("runnerd.web");
        let url = links(&file).remove(0);
        assert!(url.starts_with("http://127.0.0.1:7477/login?code="));
        assert!(auth.redeem(code(&url), Instant::now()), "the code works");
        assert!(
            !auth.redeem(code(&url), Instant::now()),
            "the code works only once"
        );
        assert_ne!(links(&file)[0], url);
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn each_address_gets_its_own_link_and_using_one_leaves_the_others_working() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(
            &dir.0,
            &["https://forge.example.ts.net:8443", "https://forge.lan"],
        );
        let before = links(&dir.0.join("runnerd.web"));
        let bases: Vec<&str> = before
            .iter()
            .map(|link| link.split_once("/login?").unwrap().0)
            .collect();
        assert_eq!(
            bases,
            [
                "http://127.0.0.1:7477",
                "https://forge.example.ts.net:8443",
                "https://forge.lan"
            ]
        );
        assert!(auth.redeem(code(&before[0]), Instant::now()));
        let after = links(&dir.0.join("runnerd.web"));
        assert_ne!(after[0], before[0], "the used link is replaced");
        assert_eq!(after[1..], before[1..], "the others stay as printed");
        assert!(
            auth.redeem(code(&before[1]), Instant::now()),
            "the proxy's link still works"
        );
    }

    #[test]
    fn a_pairing_code_works_once_and_not_after_it_expires() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &["https://forge.example.ts.net:8443"]);
        let now = Instant::now();
        let url = auth.pair("https://forge.example.ts.net:8443", now).unwrap();
        assert!(url.starts_with("https://forge.example.ts.net:8443/login?code="));
        assert!(auth.redeem(code(&url), now), "a fresh code works");
        assert!(!auth.redeem(code(&url), now), "a used code does not");
        let late = auth.pair("https://forge.example.ts.net:8443", now).unwrap();
        assert!(
            !auth.redeem(code(&late), now + PAIRING_LIFETIME + Duration::from_secs(1)),
            "an expired code does not"
        );
    }

    #[test]
    fn a_pairing_code_is_only_for_a_configured_proxy() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &["https://forge.example.ts.net:8443"]);
        assert!(
            auth.pair("https://evil.example", Instant::now()).is_err(),
            "no link on an address the owner did not list"
        );
    }

    #[test]
    fn the_session_survives_a_restart_until_every_device_is_signed_out() {
        let dir = crate::tests::TempDir::new();
        let file = dir.0.join("dashboard-session");
        let first = Sessions::open(file.clone()).unwrap();
        let cookie = first.secret();
        assert_eq!(mode(&file), 0o600);
        let restarted = Sessions::open(file.clone()).unwrap();
        assert!(restarted.is(&cookie), "the cookie works after a restart");
        restarted.sign_out_all().unwrap();
        assert!(!restarted.is(&cookie), "signing out ends it");
        assert!(
            !Sessions::open(file.clone()).unwrap().is(&cookie),
            "and it stays ended after the next restart"
        );
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn a_session_file_other_users_can_read_is_replaced() {
        let dir = crate::tests::TempDir::new();
        let file = dir.0.join("dashboard-session");
        let cookie = Sessions::open(file.clone()).unwrap().secret();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            !Sessions::open(file.clone()).unwrap().is(&cookie),
            "a secret others could read no longer signs in"
        );
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn the_log_shows_no_secret_or_code_even_when_a_new_link_cannot_be_written() {
        let dir = crate::tests::TempDir::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || Writer(writer.clone()))
            .finish();
        let mut secrets = Vec::new();
        tracing::subscriber::with_default(subscriber, || {
            let auth = dashboard(&dir.0, &["https://forge.example.ts.net:8443"]);
            secrets.push(auth.sessions.secret());
            let first = links(&dir.0.join("runnerd.web")).remove(0);
            secrets.push(code(&first).to_owned());
            // A directory in place of the login file's temporary: the new link cannot be written, and that is
            // logged.
            std::fs::create_dir(dir.0.join("runnerd.web.new")).unwrap();
            std::fs::create_dir(dir.0.join("runnerd.web.new/x")).unwrap();
            assert!(auth.redeem(code(&first), Instant::now()));
            let pairing = auth
                .pair("https://forge.example.ts.net:8443", Instant::now())
                .unwrap();
            secrets.push(code(&pairing).to_owned());
            secrets.push(lock(&auth.codes).links[0].clone());
        });
        let log = String::from_utf8(lock(&log).clone()).unwrap();
        assert!(log.contains("cannot write"), "the failure is logged: {log}");
        for secret in secrets {
            assert!(!log.contains(&secret), "the log shows a secret");
        }
    }

    struct Writer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn debug_output_shows_no_secret_or_code() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &["https://forge.example.ts.net:8443"]);
        let pairing = auth
            .pair("https://forge.example.ts.net:8443", Instant::now())
            .unwrap();
        let debug = format!("{auth:?} {:?}", auth.sessions);
        let mut secrets = vec![auth.sessions.secret(), code(&pairing).to_owned()];
        secrets.extend(
            links(&dir.0.join("runnerd.web"))
                .iter()
                .map(|link| code(link).to_owned()),
        );
        for secret in secrets {
            assert!(!debug.contains(&secret), "Debug shows a secret");
        }
    }
}
