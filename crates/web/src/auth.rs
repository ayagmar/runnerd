//! Who may use the dashboard. The port is open to every local user, so every request but the login and the static
//! assets needs the session cookie, and every write also needs an `Origin` of the dashboard itself.
//!
//! The session token is random per daemon run. A browser gets it by opening the one-time login URL, which the
//! daemon keeps in a file only its user can read (`login_file`); each use replaces the URL with a new one. This
//! module owns the token, the login code and that file; it never logs either.

use std::fmt::Write as _;
use std::io::{self, Read as _, Write as _};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use tracing::warn;

use crate::views;

/// Where the daemon serving its API on `socket` keeps the dashboard's login URL: `runnerd.sock` → `runnerd.web`.
/// `runnerctl web` reads it.
pub fn login_file(socket: &Path) -> PathBuf {
    socket.with_extension("web")
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

/// The session token, the current login code and where its URL is written.
#[derive(Debug)]
pub(crate) struct Auth {
    token: String,
    code: Mutex<String>,
    file: PathBuf,
    cookie: String,
    origins: Vec<String>,
    base: String,
}

impl Auth {
    /// A fresh token and login code for a dashboard on `addr`; writes the login URL to `file`.
    pub(crate) fn new(addr: SocketAddr, file: PathBuf) -> io::Result<Arc<Auth>> {
        let base = format!("http://{addr}");
        let mut origins = vec![base.clone()];
        if addr.ip().is_loopback() {
            origins.push(format!("http://localhost:{}", addr.port()));
        }
        let auth = Auth {
            token: random_token()?,
            code: Mutex::new(random_token()?),
            file,
            cookie: format!("runnerd-{}", addr.port()),
            origins,
            base,
        };
        auth.write_login_file(&auth.current_code())?;
        Ok(Arc::new(auth))
    }

    fn current_code(&self) -> String {
        self.code.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |code| code.clone(),
        )
    }

    /// Writes the login URL with `code` to the file, readable by this user only, replacing it atomically.
    fn write_login_file(&self, code: &str) -> io::Result<()> {
        let temporary = self.file.with_extension("web.new");
        let _ = std::fs::remove_file(&temporary);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{}/login?code={code}", self.base)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.file)
    }

    /// Takes the login code if it is the current one, and writes a new one.
    fn redeem(&self, code: &str) -> bool {
        let Ok(mut current) = self.code.lock() else {
            return false;
        };
        if !same(code, &current) {
            return false;
        }
        match random_token() {
            Ok(next) => {
                if let Err(error) = self.write_login_file(&next) {
                    warn!(
                        "cannot write the dashboard's login URL to {}: {error}",
                        self.file.display()
                    );
                }
                *current = next;
            }
            Err(error) => warn!("cannot make a new dashboard login code: {error}"),
        }
        true
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
            .any(|(name, value)| name == self.cookie && same(value, &self.token))
    }

    fn same_origin(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::ORIGIN)
            .and_then(|origin| origin.to_str().ok())
            .is_some_and(|origin| self.origins.iter().any(|allowed| allowed == origin))
    }
}

#[derive(Deserialize)]
pub(crate) struct Login {
    code: String,
}

/// `GET /login?code=…`: a valid code sets the session cookie and goes to the overview.
pub(crate) async fn login(State(auth): State<Arc<Auth>>, Query(login): Query<Login>) -> Response {
    if !auth.redeem(&login.code) {
        return (StatusCode::UNAUTHORIZED, views::signed_out(true)).into_response();
    }
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict",
        auth.cookie, auth.token
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

/// Lets through the login, the assets, and requests with the session cookie; writes also need the dashboard's own
/// `Origin`. A missing or wrong cookie is 401, a foreign origin 403.
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
        return (StatusCode::UNAUTHORIZED, views::signed_out(false)).into_response();
    }
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && !auth.same_origin(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "writes must come from the dashboard's own pages",
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_code_works_once_and_the_file_then_holds_a_new_one() {
        let dir = crate::tests::TempDir::new();
        let file = dir.0.join("runnerd.web");
        let auth = Auth::new("127.0.0.1:7477".parse().unwrap(), file.clone()).unwrap();
        let url = std::fs::read_to_string(&file).unwrap();
        let code = url.trim().split_once("code=").unwrap().1.to_owned();
        assert!(url.starts_with("http://127.0.0.1:7477/login?code="));
        assert!(auth.redeem(&code));
        assert!(!auth.redeem(&code));
        let next = std::fs::read_to_string(&file).unwrap();
        assert_ne!(url, next);
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&file).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
    }
}
