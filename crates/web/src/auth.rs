//! Who may use the dashboard. The port is open to every local user, so every request but the login and the static
//! assets needs the session cookie, and every write also needs an `Origin` of the dashboard itself: its loopback
//! address, or one of `[web] origins`, the addresses of a reverse proxy in front of it.
//!
//! `Sessions` owns every credential, under one lock: the session secret the cookie holds, which the daemon keeps in
//! a file only its user can read, so a browser stays signed in across restarts; the one-time login links of the
//! running dashboard, one per address in `login_file`, each replaced once used; and pairing codes, which sign one
//! device in on the one origin they were made for, within `PAIRING_LIFETIME`. Signing every browser out replaces
//! the secret and every code at once. This module never logs any of them.

use std::fmt::{self, Write as _};
use std::io::{self, Read as _, Write as _};
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use runnerd_core::config::Origin;
use rustix::fs::{Mode, OFlags};
use serde::Deserialize;
use tracing::warn;

use crate::views::{self, SignedOut};

/// How long a signed-in browser stays signed in without using the dashboard. Each authenticated response sets the
/// cookie again, so a phone opened once a month never signs out; one lost in a drawer does after this.
const SESSION_LIFETIME: Duration = Duration::from_hours(30 * 24);

/// How long a pairing code works: enough to pick up a phone and scan it.
pub const PAIRING_LIFETIME: Duration = Duration::from_mins(5);

/// The most pairing codes alive at once; making another drops the oldest.
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

/// Fails unless `dir` is owned by this user or root and no other user can write to it: another user could
/// otherwise swap the secret's file for one they know.
fn check_private_dir(dir: &Path) -> io::Result<()> {
    let metadata = std::fs::metadata(dir)?;
    let owner = metadata.uid();
    let writable_by_others = metadata.mode() & 0o022;
    if (owner != rustix::process::geteuid().as_raw() && owner != 0) || writable_by_others != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} can be written by other users; keep the job history (history.database) in a directory only \
                 you can write",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// The secret in `file`, or none to make a new one: missing, malformed, or readable or owned by another user. A
/// symbolic link or anything but a regular file is an error. The checks and the read use one descriptor, opened
/// without following a link, so the file cannot be swapped between them.
fn read_secret(file: &Path) -> io::Result<Option<String>> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let descriptor = match rustix::fs::open(file, flags, Mode::empty()) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is a symbolic link; remove it", file.display()),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let opened = std::fs::File::from(descriptor);
    let metadata = opened.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular file; remove it", file.display()),
        ));
    }
    let shared = metadata.mode() & 0o077;
    if metadata.uid() != rustix::process::geteuid().as_raw() || shared != 0 {
        warn!(
            "{} is readable by or belongs to another user; signing every browser out",
            file.display()
        );
        return Ok(None);
    }
    let mut text = String::new();
    opened.take(256).read_to_string(&mut text)?;
    let secret = text.trim();
    Ok(
        (secret.len() == 64 && secret.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| secret.to_owned()),
    )
}

/// The origin a request came to: `X-Forwarded-Proto` and `X-Forwarded-Host` from a proxy, or `http` and `Host`,
/// as a browser would serialize it; none when they do not make an origin.
fn request_origin(headers: &HeaderMap) -> Option<Origin> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let scheme = header("x-forwarded-proto").unwrap_or("http");
    let host = header("x-forwarded-host").or_else(|| header(header::HOST.as_str()))?;
    format!("{scheme}://{host}").parse().ok()
}

/// A pairing code: it works once, on `origin`, until `expires`.
struct Pairing {
    code: String,
    origin: String,
    expires: Instant,
}

/// A running dashboard's login links: the file they are written to, and an address and a code per link, the
/// loopback address first.
struct Links {
    file: PathBuf,
    bases: Vec<String>,
    codes: Vec<String>,
}

impl Links {
    fn write(&self) -> io::Result<()> {
        let text =
            self.bases
                .iter()
                .zip(&self.codes)
                .fold(String::new(), |mut text, (base, code)| {
                    let _ = writeln!(text, "{base}/login?code={code}");
                    text
                });
        write_private(&self.file, &text)
    }

    /// New codes for every link, written to the file.
    fn renew(&mut self) {
        match self.bases.iter().map(|_| random_token()).collect() {
            Ok(codes) => self.codes = codes,
            // The old codes go anyway: an empty one matches nothing.
            Err(error) => {
                self.codes = vec![String::new(); self.bases.len()];
                warn!("cannot make new dashboard login codes: {error}");
            }
        }
        if let Err(error) = self.write() {
            warn!(
                "cannot write the dashboard's login links to {}: {error}",
                self.file.display()
            );
        }
    }

    /// `[web] origins`: the addresses after the loopback one.
    fn proxies(&self) -> &[String] {
        self.bases.get(1..).unwrap_or_default()
    }
}

/// The secret and the codes, changed together.
struct Credentials {
    secret: String,
    /// None while no dashboard runs.
    links: Option<Links>,
    pairings: Vec<Pairing>,
}

/// Why no pairing link was made.
#[derive(Debug)]
pub enum PairError {
    /// The daemon serves no dashboard (`--web off`).
    DashboardOff,
    /// `[web] origins` is empty: a phone cannot reach loopback.
    NoOrigins,
    /// The origin asked for is not one of `[web] origins`.
    NotListed(String),
    /// No random code could be read.
    Io(io::Error),
}

impl fmt::Display for PairError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairError::DashboardOff => formatter.write_str("runnerd serves no dashboard (--web off)"),
            PairError::NoOrigins => formatter.write_str(
                "no address another device reaches the dashboard on: list it in `[web] origins` in runnerd.toml \
                 and restart runnerd",
            ),
            PairError::NotListed(origin) => write!(formatter, "{origin} is not one of [web] origins"),
            PairError::Io(error) => write!(formatter, "cannot make a pairing code: {error}"),
        }
    }
}

impl std::error::Error for PairError {}

/// Every credential of the dashboard: the session secret, kept in a file only the daemon's user can read, which
/// every signed-in browser holds in its cookie; the login links of a running dashboard; and the pairing codes.
pub struct Sessions {
    file: PathBuf,
    credentials: Mutex<Credentials>,
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
    /// Fails when other users can write to its directory, or `file` is a symbolic link or not a regular file.
    pub fn open(file: PathBuf) -> io::Result<Arc<Sessions>> {
        let dir = match file.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        check_private_dir(dir)?;
        let secret = if let Some(secret) = read_secret(&file)? {
            secret
        } else {
            let secret = random_token()?;
            write_private(&file, &format!("{secret}\n"))?;
            secret
        };
        Ok(Arc::new(Sessions {
            file,
            credentials: Mutex::new(Credentials {
                secret,
                links: None,
                pairings: Vec::new(),
            }),
        }))
    }

    /// Replaces the secret, so every browser signed in is signed out, and every login link and pairing code made
    /// before, so none signs one in again; the login file gets new links.
    pub fn sign_out_all(&self) -> io::Result<()> {
        let mut credentials = lock(&self.credentials);
        let next = random_token()?;
        write_private(&self.file, &format!("{next}\n"))?;
        credentials.secret = next;
        credentials.pairings.clear();
        if let Some(links) = &mut credentials.links {
            links.renew();
        }
        Ok(())
    }

    /// A link on `origin`, one of `[web] origins`, that signs one device in there within `PAIRING_LIFETIME` of
    /// `now`.
    pub fn pair(&self, origin: &str, now: Instant) -> Result<String, PairError> {
        let mut credentials = lock(&self.credentials);
        let links = credentials.links.as_ref().ok_or(PairError::DashboardOff)?;
        if links.proxies().is_empty() {
            return Err(PairError::NoOrigins);
        }
        if !links.proxies().iter().any(|proxy| proxy == origin) {
            return Err(PairError::NotListed(origin.to_owned()));
        }
        let code = random_token().map_err(PairError::Io)?;
        credentials.pairings.retain(|pairing| pairing.expires > now);
        if credentials.pairings.len() >= PAIRING_MAX {
            credentials.pairings.remove(0);
        }
        credentials.pairings.push(Pairing {
            code: code.clone(),
            origin: origin.to_owned(),
            expires: now + PAIRING_LIFETIME,
        });
        Ok(format!("{origin}/login?code={code}"))
    }

    /// `[web] origins` of the running dashboard; none when it does not run.
    pub fn origins(&self) -> Option<Vec<String>> {
        lock(&self.credentials)
            .links
            .as_ref()
            .map(|links| links.proxies().to_vec())
    }

    /// Makes a login link for each of `bases`, the loopback address first, and writes them to `file`.
    fn publish(&self, file: PathBuf, bases: Vec<String>) -> io::Result<()> {
        let codes = bases
            .iter()
            .map(|_| random_token())
            .collect::<io::Result<_>>()?;
        let links = Links { file, bases, codes };
        links.write()?;
        lock(&self.credentials).links = Some(links);
        Ok(())
    }

    /// Drops the login links and their file, when the dashboard stops.
    fn withdraw(&self) {
        if let Some(links) = lock(&self.credentials).links.take() {
            let _ = std::fs::remove_file(&links.file);
        }
    }

    /// The secret, if `value` is it: what a renewed cookie carries, so a request that passed with a secret since
    /// replaced never gets the new one.
    fn authenticate(&self, value: &str) -> Option<String> {
        let credentials = lock(&self.credentials);
        same(value, &credentials.secret).then(|| credentials.secret.clone())
    }

    /// Takes `code`, a login link's on any address, or a pairing code alive at `now` on `origin`, the one it was
    /// made for, and returns the secret to sign in with. A used link's code is replaced in the login file.
    fn redeem(&self, code: &str, origin: Option<&Origin>, now: Instant) -> Option<String> {
        let mut credentials = lock(&self.credentials);
        credentials.pairings.retain(|pairing| pairing.expires > now);
        if let Some(index) = credentials
            .pairings
            .iter()
            .position(|pairing| same(code, &pairing.code))
        {
            if origin.is_none_or(|origin| origin.as_str() != credentials.pairings[index].origin) {
                return None;
            }
            credentials.pairings.remove(index);
            return Some(credentials.secret.clone());
        }
        let links = credentials.links.as_mut()?;
        let index = links.codes.iter().position(|link| same(code, link))?;
        match random_token() {
            Ok(next) => {
                links.codes[index] = next;
                if let Err(error) = links.write() {
                    warn!(
                        "cannot write the dashboard's login links to {}: {error}",
                        links.file.display()
                    );
                }
            }
            // The used code goes anyway: a code works once even when no new one can be made.
            Err(error) => {
                links.codes[index] = String::new();
                warn!("cannot make a new dashboard login code: {error}");
            }
        }
        Some(credentials.secret.clone())
    }
}

/// The checks of a running dashboard: its cookie's name and the origins its pages may write from.
#[derive(Debug)]
pub(crate) struct Auth {
    sessions: Arc<Sessions>,
    cookie: String,
    /// The origins whose pages may write: the loopback address, `localhost` on its port, and the proxies.
    origins: Vec<String>,
    proxies: Vec<String>,
}

impl Auth {
    /// The checks for a dashboard on `addr`, which also takes writes from `proxies`; writes a login link for each
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
        sessions.publish(
            file,
            std::iter::once(base)
                .chain(proxies.iter().cloned())
                .collect(),
        )?;
        Ok(Arc::new(Auth {
            sessions,
            cookie: format!("runnerd-{}", addr.port()),
            origins,
            proxies,
        }))
    }

    /// `[web] origins`, the addresses a QR code can sign a phone in on.
    pub(crate) fn proxies(&self) -> &[String] {
        &self.proxies
    }

    /// Removes the login file, when the dashboard stops.
    pub(crate) fn remove_login_file(&self) {
        self.sessions.withdraw();
    }

    /// The secret the request's session cookie holds, if it is the current one.
    fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .filter(|(name, _)| *name == self.cookie)
            .find_map(|(_, value)| self.sessions.authenticate(value))
    }

    fn same_origin(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::ORIGIN)
            .and_then(|origin| origin.to_str().ok())
            .is_some_and(|origin| self.origins.iter().any(|allowed| allowed == origin))
    }

    /// The session cookie holding `secret`. Through a proxy that says it took the request over https
    /// (`X-Forwarded-Proto`, as `tailscale serve` sends), it is `Secure`.
    fn cookie(&self, headers: &HeaderMap, secret: &str) -> Option<HeaderValue> {
        let https = headers
            .get("x-forwarded-proto")
            .is_some_and(|proto| proto.as_bytes().eq_ignore_ascii_case(b"https"));
        HeaderValue::try_from(format!(
            "{}={secret}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{}",
            self.cookie,
            SESSION_LIFETIME.as_secs(),
            if https { "; Secure" } else { "" }
        ))
        .ok()
    }
}

/// A response carrying a credential, which no cache may keep.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
pub(crate) struct Login {
    code: String,
}

/// `GET /login?code=…`: a live code sets the session cookie and goes to the overview. A pairing code works only on
/// the origin it was made for, as `X-Forwarded-Proto` and `X-Forwarded-Host` or `Host` name it.
pub(crate) async fn login(
    State(auth): State<Arc<Auth>>,
    Query(login): Query<Login>,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let Some(secret) = auth
        .sessions
        .redeem(&login.code, origin.as_ref(), Instant::now())
    else {
        return (
            StatusCode::UNAUTHORIZED,
            views::signed_out(SignedOut::CodeUsed),
        )
            .into_response();
    };
    let mut response = Redirect::to("/").into_response();
    if let Some(cookie) = auth.cookie(&headers, &secret) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    no_store(response)
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
    let markup = match auth.sessions.pair(&origin, Instant::now()) {
        Ok(url) => views::config::pairing_code(&url, &origin),
        Err(error) => views::notice(&error.to_string(), views::Tone::Failed),
    };
    no_store(axum::response::Html(markup.into_string()).into_response())
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
    let Some(secret) = auth.authenticate(request.headers()) else {
        return (
            StatusCode::UNAUTHORIZED,
            views::signed_out(SignedOut::NoSession),
        )
            .into_response();
    };
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && !auth.same_origin(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "writes must come from the dashboard's own pages",
        )
            .into_response();
    }
    let cookie = auth.cookie(request.headers(), &secret);
    let mut response = next.run(request).await;
    if let Some(cookie) = cookie {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const PROXY: &str = "https://forge.example.ts.net:8443";

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
        std::fs::symlink_metadata(file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    fn origin(text: &str) -> Origin {
        text.parse().unwrap()
    }

    /// The secret in the session file.
    fn stored(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("dashboard-session"))
            .unwrap()
            .trim()
            .to_owned()
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
        let now = Instant::now();
        assert!(
            auth.sessions.redeem(code(&url), None, now).is_some(),
            "the code works"
        );
        assert!(
            auth.sessions.redeem(code(&url), None, now).is_none(),
            "the code works only once"
        );
        assert_ne!(links(&file)[0], url);
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn each_address_gets_its_own_link_and_using_one_leaves_the_others_working() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[PROXY, "https://forge.lan"]);
        let before = links(&dir.0.join("runnerd.web"));
        let bases: Vec<&str> = before
            .iter()
            .map(|link| link.split_once("/login?").unwrap().0)
            .collect();
        assert_eq!(bases, ["http://127.0.0.1:7477", PROXY, "https://forge.lan"]);
        let now = Instant::now();
        assert!(auth.sessions.redeem(code(&before[0]), None, now).is_some());
        let after = links(&dir.0.join("runnerd.web"));
        assert_ne!(after[0], before[0], "the used link is replaced");
        assert_eq!(after[1..], before[1..], "the others stay as printed");
        assert!(
            auth.sessions.redeem(code(&before[1]), None, now).is_some(),
            "the proxy's link still works"
        );
    }

    #[test]
    fn a_pairing_code_works_once_and_not_after_it_expires() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[PROXY]);
        let now = Instant::now();
        let url = auth.sessions.pair(PROXY, now).unwrap();
        assert!(url.starts_with("https://forge.example.ts.net:8443/login?code="));
        let on_proxy = Some(origin(PROXY));
        assert!(
            auth.sessions
                .redeem(code(&url), on_proxy.as_ref(), now)
                .is_some(),
            "a fresh code works"
        );
        assert!(
            auth.sessions
                .redeem(code(&url), on_proxy.as_ref(), now)
                .is_none(),
            "a used code does not"
        );
        let late = auth.sessions.pair(PROXY, now).unwrap();
        let after = now + PAIRING_LIFETIME + Duration::from_secs(1);
        assert!(
            auth.sessions
                .redeem(code(&late), on_proxy.as_ref(), after)
                .is_none(),
            "an expired code does not"
        );
    }

    #[test]
    fn a_pairing_code_works_only_on_the_origin_it_was_made_for() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[PROXY, "https://forge.lan"]);
        let now = Instant::now();
        let url = auth.sessions.pair(PROXY, now).unwrap();
        for elsewhere in [
            Some(origin("http://127.0.0.1:7477")),
            Some(origin("https://forge.lan")),
            Some(origin("https://unlisted.example")),
            None,
        ] {
            assert!(
                auth.sessions
                    .redeem(code(&url), elsewhere.as_ref(), now)
                    .is_none(),
                "the code does not work on {elsewhere:?}"
            );
        }
        assert!(
            auth.sessions
                .redeem(code(&url), Some(&origin(PROXY)), now)
                .is_some(),
            "nor is it spent there"
        );
    }

    #[test]
    fn a_pairing_code_is_only_for_a_configured_proxy() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[PROXY]);
        assert!(
            matches!(
                auth.sessions.pair("https://evil.example", Instant::now()),
                Err(PairError::NotListed(_))
            ),
            "no link on an address the owner did not list"
        );
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[]);
        assert!(
            matches!(
                auth.sessions.pair(PROXY, Instant::now()),
                Err(PairError::NoOrigins)
            ),
            "none without [web] origins"
        );
        let sessions = Sessions::open(dir.0.join("other-session")).unwrap();
        assert!(
            matches!(
                sessions.pair(PROXY, Instant::now()),
                Err(PairError::DashboardOff)
            ),
            "none while no dashboard runs"
        );
    }

    #[test]
    fn at_most_eight_pairing_codes_live_at_once() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[PROXY]);
        let now = Instant::now();
        let urls: Vec<String> = (0..9)
            .map(|_| auth.sessions.pair(PROXY, now).unwrap())
            .collect();
        let on_proxy = Some(origin(PROXY));
        assert!(
            auth.sessions
                .redeem(code(&urls[0]), on_proxy.as_ref(), now)
                .is_none(),
            "the oldest is dropped"
        );
        assert!(
            auth.sessions
                .redeem(code(&urls[1]), on_proxy.as_ref(), now)
                .is_some(),
            "the next still works"
        );
    }

    #[test]
    fn the_session_survives_a_restart_until_every_device_is_signed_out() {
        let dir = crate::tests::TempDir::new();
        let file = dir.0.join("dashboard-session");
        Sessions::open(file.clone()).unwrap();
        let cookie = stored(&dir.0);
        assert_eq!(mode(&file), 0o600);
        let restarted = Sessions::open(file.clone()).unwrap();
        assert!(
            restarted.authenticate(&cookie).is_some(),
            "the cookie works after a restart"
        );
        restarted.sign_out_all().unwrap();
        assert!(
            restarted.authenticate(&cookie).is_none(),
            "signing out ends it"
        );
        assert!(
            Sessions::open(file.clone())
                .unwrap()
                .authenticate(&cookie)
                .is_none(),
            "and it stays ended after the next restart"
        );
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn signing_every_device_out_ends_every_code_made_before_and_writes_new_links() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[PROXY]);
        let now = Instant::now();
        let pairing = auth.sessions.pair(PROXY, now).unwrap();
        let printed = links(&dir.0.join("runnerd.web"));
        auth.sessions.sign_out_all().unwrap();
        let on_proxy = Some(origin(PROXY));
        assert!(
            auth.sessions
                .redeem(code(&pairing), on_proxy.as_ref(), now)
                .is_none(),
            "a QR code shown before no longer signs in"
        );
        for link in &printed {
            assert!(
                auth.sessions.redeem(code(link), None, now).is_none(),
                "a link printed before no longer signs in"
            );
        }
        let fresh = links(&dir.0.join("runnerd.web"));
        assert_ne!(fresh, printed);
        let secret = auth.sessions.redeem(code(&fresh[0]), None, now).unwrap();
        assert!(
            auth.sessions.authenticate(&secret).is_some(),
            "a new link signs in with the new secret"
        );
    }

    #[test]
    fn a_request_that_passed_before_a_sign_out_is_renewed_with_its_old_secret_only() {
        let dir = crate::tests::TempDir::new();
        let auth = dashboard(&dir.0, &[]);
        let old = stored(&dir.0);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("{}={old}", auth.cookie).parse().unwrap(),
        );
        // The middleware's order: authenticate, the handler runs while every device is signed out, renew.
        let passed = auth.authenticate(&headers).unwrap();
        auth.sessions.sign_out_all().unwrap();
        let renewed = auth.cookie(&headers, &passed).unwrap();
        let renewed = renewed.to_str().unwrap();
        let new = stored(&dir.0);
        assert!(
            renewed.contains(&old) && !renewed.contains(&new),
            "the renewed cookie carries the secret the request came with, not its replacement"
        );
        assert!(
            auth.authenticate(&headers).is_none(),
            "and the next request is signed out"
        );
    }

    #[test]
    fn a_session_file_other_users_can_read_is_replaced() {
        let dir = crate::tests::TempDir::new();
        let file = dir.0.join("dashboard-session");
        Sessions::open(file.clone()).unwrap();
        let cookie = stored(&dir.0);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            Sessions::open(file.clone())
                .unwrap()
                .authenticate(&cookie)
                .is_none(),
            "a secret others could read no longer signs in"
        );
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn a_session_file_that_is_a_symbolic_link_or_a_pipe_is_refused() {
        let dir = crate::tests::TempDir::new();
        let known = dir.0.join("known");
        std::fs::write(&known, format!("{}\n", "ab".repeat(32))).unwrap();
        std::fs::set_permissions(&known, std::fs::Permissions::from_mode(0o600)).unwrap();
        let file = dir.0.join("dashboard-session");
        std::os::unix::fs::symlink(&known, &file).unwrap();
        let error = Sessions::open(file).unwrap_err();
        assert!(
            error.to_string().contains("symbolic link"),
            "a known secret behind a link is not taken"
        );
        let fifo = dir.0.join("fifo-session");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        let error = Sessions::open(fifo).unwrap_err();
        assert!(
            error.to_string().contains("not a regular file"),
            "a pipe is refused without blocking"
        );
    }

    #[test]
    fn a_session_file_in_a_directory_others_can_write_is_refused() {
        let dir = crate::tests::TempDir::new();
        let shared = dir.0.join("shared");
        std::fs::create_dir(&shared).unwrap();
        for unsafe_mode in [0o777, 0o1777, 0o770] {
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(unsafe_mode))
                .unwrap();
            let error = Sessions::open(shared.join("dashboard-session")).unwrap_err();
            assert!(
                error.to_string().contains("can be written by other users"),
                "mode {unsafe_mode:o} is refused"
            );
        }
        assert!(
            !shared.join("dashboard-session").exists(),
            "nothing is written there"
        );
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
            let auth = dashboard(&dir.0, &[PROXY]);
            let first = links(&dir.0.join("runnerd.web")).remove(0);
            secrets.push(code(&first).to_owned());
            // A directory in place of the login file's temporary: the new link cannot be written, and that is
            // logged.
            std::fs::create_dir(dir.0.join("runnerd.web.new")).unwrap();
            std::fs::create_dir(dir.0.join("runnerd.web.new/x")).unwrap();
            let secret = auth
                .sessions
                .redeem(code(&first), None, Instant::now())
                .unwrap();
            secrets.push(secret);
            let pairing = auth.sessions.pair(PROXY, Instant::now()).unwrap();
            secrets.push(code(&pairing).to_owned());
            auth.sessions.sign_out_all().unwrap();
            let credentials = lock(&auth.sessions.credentials);
            secrets.push(credentials.secret.clone());
            secrets.extend(credentials.links.as_ref().unwrap().codes.iter().cloned());
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
        let auth = dashboard(&dir.0, &[PROXY]);
        let pairing = auth.sessions.pair(PROXY, Instant::now()).unwrap();
        let debug = format!("{auth:?} {:?}", auth.sessions);
        let mut secrets = vec![stored(&dir.0), code(&pairing).to_owned()];
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
