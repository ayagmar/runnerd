//! A GitHub App installation's credentials (ADR 0182): the App's private key read from its file, the RS256 JWT it
//! signs, and the installation token that JWT is exchanged for. The exchange has a client of its own: it never goes
//! through `Github::exchange`, follows no redirect, ends within 10 s, and what GitHub answers
//! to it never reaches the installation's rate-limit reading, its counts, its backoff or its kept answers. The
//! doctor's [`App::probe`] reads, with a token minted for it, which permissions the installation has and which
//! repositories it covers, and sends nothing that writes.
//!
//! It must not put the key, a JWT or a token in a log line, a `Debug` output or an error, nor GitHub's own words or
//! anything else it sent back: an error says what failed and GitHub's status, no more.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::FutureExt as _;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use ring::rand::SystemRandom;
use ring::rsa::KeyPair;
use ring::signature::RSA_PKCS1_SHA256;
use serde::Deserialize;

use super::{Error, ExpiringSource, ExpiringToken, MAX_PAGES, PER_PAGE, REQUEST_TIMEOUT, Token};
use crate::api::Visibility;
use crate::config::GithubApp;
use crate::time;

/// The largest key file read: a GitHub App's 2048-bit key is under 2 KB, a 4096-bit one under 4 KB.
pub const KEY_BYTES_MOST: u64 = 64 * 1024;

/// How far back a JWT's `iat` is set, for a clock behind GitHub's: GitHub's own advice.
const JWT_BACKDATE: Duration = Duration::from_secs(60);

/// How long a JWT lasts after it is signed: under GitHub's 10 minutes, for a clock ahead of GitHub's.
const JWT_LIFETIME: Duration = Duration::from_secs(540);

/// The repository permissions every feature of runnerd needs, and the level each needs (ADR 0182). Actions: write is
/// needed even with `[github] job_actions = false`: a forced pause cancels runs. Metadata: read comes with any App.
pub const REQUIRED_PERMISSIONS: [(&str, Level); 4] = [
    ("administration", Level::Write),
    ("actions", Level::Write),
    ("contents", Level::Read),
    ("pull_requests", Level::Read),
];

/// What a permission lets an App do, as GitHub names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// `read`.
    Read,
    /// `write`, which includes read.
    Write,
}

impl Level {
    /// The level GitHub's word names: `read` or `write`; `admin` is more than write.
    pub fn of(word: &str) -> Option<Level> {
        match word {
            "read" => Some(Level::Read),
            "write" | "admin" => Some(Level::Write),
            _ => None,
        }
    }

    /// GitHub's word for it.
    pub fn word(self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Write => "write",
        }
    }
}

/// Why a key file was not used. It names the file, never what it holds.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The file cannot be opened.
    #[error("cannot open {}: {source}", path.display())]
    Open {
        /// The key's file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The path's last component is a symbolic link.
    #[error("{} is a symbolic link; name the key's file itself", path.display())]
    Symlink {
        /// The key's file.
        path: PathBuf,
    },
    /// Not a regular file.
    #[error("{} is not a regular file", path.display())]
    NotFile {
        /// The key's file.
        path: PathBuf,
    },
    /// Owned by another user than the daemon's.
    #[error("{} is owned by uid {owner}, not by runnerd's user (uid {user})", path.display())]
    Owner {
        /// The key's file.
        path: PathBuf,
        /// Its owner.
        owner: u32,
        /// The daemon's effective user.
        user: u32,
    },
    /// Others than its owner may read or write it.
    #[error("{} has mode {mode:04o}; only its owner may read it: chmod 600 {}", path.display(), path.display())]
    Shared {
        /// The key's file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// Larger than any App key.
    #[error("{} is larger than {KEY_BYTES_MOST} bytes, which no GitHub App key is", path.display())]
    TooLarge {
        /// The key's file.
        path: PathBuf,
    },
    /// What it holds is not a key runnerd takes: why, in words of its own.
    #[error("{}: {why}", path.display())]
    Unusable {
        /// The key's file.
        path: PathBuf,
        /// Why, never a byte of the file.
        why: &'static str,
    },
}

/// A GitHub App's private RSA key. Its `Debug` output shows nothing of it.
pub struct AppKey(KeyPair);

impl fmt::Debug for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppKey(<redacted>)")
    }
}

impl AppKey {
    /// The key in the file at `path`, as GitHub issues it: PEM, PKCS#1 (`RSA PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`),
    /// not encrypted. The file is opened without following a symbolic link at its end, and must be a regular file
    /// the daemon's user owns that no other user may read or write, of at most [`KEY_BYTES_MOST`]. Its mode is never
    /// changed here.
    pub fn load(path: &Path) -> Result<AppKey, KeyError> {
        use rustix::fs::{Mode, OFlags};
        let flags =
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let opened = rustix::fs::open(path, flags, Mode::empty()).map_err(|errno| {
            if errno == rustix::io::Errno::LOOP {
                KeyError::Symlink {
                    path: path.to_owned(),
                }
            } else {
                KeyError::Open {
                    path: path.to_owned(),
                    source: errno.into(),
                }
            }
        })?;
        let file = File::from(opened);
        let metadata = file.metadata().map_err(|source| KeyError::Open {
            path: path.to_owned(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(KeyError::NotFile {
                path: path.to_owned(),
            });
        }
        let user = rustix::process::geteuid().as_raw();
        if metadata.uid() != user {
            return Err(KeyError::Owner {
                path: path.to_owned(),
                owner: metadata.uid(),
                user,
            });
        }
        let mode = metadata.mode() & 0o7777;
        if mode & 0o077 != 0 {
            return Err(KeyError::Shared {
                path: path.to_owned(),
                mode,
            });
        }
        if metadata.len() > KEY_BYTES_MOST {
            return Err(KeyError::TooLarge {
                path: path.to_owned(),
            });
        }
        let mut pem = Vec::new();
        file.take(KEY_BYTES_MOST + 1)
            .read_to_end(&mut pem)
            .map_err(|source| KeyError::Open {
                path: path.to_owned(),
                source,
            })?;
        if pem.len() as u64 > KEY_BYTES_MOST {
            return Err(KeyError::TooLarge {
                path: path.to_owned(),
            });
        }
        AppKey::parse(&pem).map_err(|why| KeyError::Unusable {
            path: path.to_owned(),
            why,
        })
    }

    /// The key a PEM file holds, or why it is not one runnerd takes.
    fn parse(pem: &[u8]) -> Result<AppKey, &'static str> {
        let text = std::str::from_utf8(pem).map_err(|_| "not a PEM file")?;
        // One PEM block and nothing else, but the whitespace around it.
        let mut lines = text.trim().lines().map(str::trim);
        let label = lines
            .next()
            .and_then(|line| line.strip_prefix("-----BEGIN "))
            .and_then(|rest| rest.strip_suffix("-----"))
            .ok_or(if text.contains("-----BEGIN ") {
                "text before the PEM block; the file must hold the key alone"
            } else {
                "not a PEM file"
            })?;
        let end = format!("-----END {label}-----");
        let mut body = String::new();
        let mut ended = false;
        for line in lines.by_ref() {
            if line == end {
                ended = true;
                break;
            }
            // RFC 1421's headers, which only an encrypted PKCS#1 key carries.
            if line.contains(':') {
                return Err("the key is encrypted; GitHub's keys are not, so download a new one");
            }
            body.push_str(line);
        }
        if !ended {
            return Err("the PEM block does not end");
        }
        if lines.next().is_some() {
            return Err("text after the PEM block; the file must hold the key alone");
        }
        let encoding = match label {
            "RSA PRIVATE KEY" => Encoding::Pkcs1,
            "PRIVATE KEY" => Encoding::Pkcs8,
            "ENCRYPTED PRIVATE KEY" => {
                return Err("the key is encrypted; GitHub's keys are not, so download a new one");
            }
            _ => return Err("not an RSA private key"),
        };
        let der = STANDARD
            .decode(body.as_bytes())
            .map_err(|_| "the PEM block is not base64")?;
        let key = match encoding {
            Encoding::Pkcs1 => KeyPair::from_der(&der),
            Encoding::Pkcs8 => KeyPair::from_pkcs8(&der),
        }
        .map_err(|_| "not an RSA private key of 2048 to 8192 bits")?;
        Ok(AppKey(key))
    }

    /// A JWT that says it is App `app_id`'s, signed with RS256 `now`: issued a minute before `now`, for a clock behind
    /// GitHub's, and lasting nine minutes.
    fn jwt(&self, app_id: u64, now: SystemTime) -> Result<Jwt, Error> {
        let issued = now
            .checked_sub(JWT_BACKDATE)
            .and_then(|issued| issued.duration_since(SystemTime::UNIX_EPOCH).ok())
            .ok_or_else(|| Error::Token("the clock is before 1970".to_owned()))?
            .as_secs();
        let expires = issued + (JWT_BACKDATE + JWT_LIFETIME).as_secs();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD
            .encode(format!(r#"{{"iat":{issued},"exp":{expires},"iss":"{app_id}"}}"#).as_bytes());
        let signed = format!("{header}.{claims}");
        let mut signature = vec![0; self.0.public().modulus_len()];
        self.0
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(|_| Error::Token("cannot sign the GitHub App's JWT".to_owned()))?;
        Ok(Jwt(format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }
}

/// How a PEM block encodes an RSA key.
enum Encoding {
    Pkcs1,
    Pkcs8,
}

/// A signed JWT. Its `Debug` output never shows it.
struct Jwt(String);

impl fmt::Debug for Jwt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Jwt(<redacted>)")
    }
}

/// A GitHub App installation, with its key: what mints the installation's tokens.
pub struct App {
    /// The App's ID, which its JWT names.
    id: u64,
    /// The installation's ID, which its tokens are minted for.
    installation: u64,
    key: AppKey,
    http: reqwest::Client,
    base: String,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("id", &self.id)
            .field("installation", &self.installation)
            .finish_non_exhaustive()
    }
}

/// An installation token as GitHub minted it, with what the installation may do.
struct Minted {
    token: ExpiringToken,
    /// Each permission GitHub listed, as runnerd recognizes its level.
    permissions: BTreeMap<String, Granted>,
}

/// `POST /app/installations/{id}/access_tokens`'s answer. Never `Debug`: it holds the token.
#[derive(Deserialize)]
struct AccessToken {
    token: String,
    expires_at: String,
    #[serde(default)]
    permissions: BTreeMap<String, serde_json::Value>,
}

/// A page of `GET /installation/repositories`.
#[derive(Deserialize)]
struct Repositories {
    repositories: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    full_name: String,
    #[serde(default)]
    visibility: Option<String>,
}

/// What the doctor learns of an installation, with a token minted for it and then dropped: no credential, and no text
/// GitHub sent, only what runnerd recognized in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// What the installation has of each of [`REQUIRED_PERMISSIONS`], by its name there.
    pub permissions: BTreeMap<&'static str, Granted>,
    /// Each repository asked about, by the name it was asked by: whether the installation covers it, and its
    /// visibility when it does.
    pub repositories: BTreeMap<String, Coverage>,
}

/// What an installation has of a permission, as runnerd recognizes GitHub's word for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granted {
    /// Nothing: GitHub does not list it.
    Nothing,
    /// This level.
    Level(Level),
    /// A word runnerd does not know, never repeated.
    Unrecognized,
}

impl Granted {
    /// What GitHub's `word`, if it gave one, grants.
    fn of(word: Option<&str>) -> Granted {
        match word {
            None => Granted::Nothing,
            Some(word) => Level::of(word).map_or(Granted::Unrecognized, Granted::Level),
        }
    }

    /// It in a word or two, such as `read` or `none`.
    pub fn words(self) -> &'static str {
        match self {
            Granted::Nothing => "none",
            Granted::Level(level) => level.word(),
            Granted::Unrecognized => "a level runnerd does not know",
        }
    }
}

/// Whether an installation covers a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// It does; with the repository's visibility, when GitHub gave one runnerd recognizes.
    Covered(Option<Visibility>),
    /// It does not: runnerd cannot register runners for it.
    Uncovered,
}

impl Installation {
    /// The required permissions the installation lacks or has below the level needed, with what it has.
    pub fn missing(&self) -> Vec<Missing> {
        REQUIRED_PERMISSIONS
            .iter()
            .filter_map(|&(name, needed)| {
                let has = self
                    .permissions
                    .get(name)
                    .copied()
                    .unwrap_or(Granted::Nothing);
                let enough = matches!(has, Granted::Level(level) if level >= needed);
                (!enough).then_some(Missing { name, needed, has })
            })
            .collect()
    }

    /// The repositories asked about that the installation does not cover.
    pub fn uncovered(&self) -> Vec<&str> {
        self.repositories
            .iter()
            .filter(|(_, coverage)| **coverage == Coverage::Uncovered)
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// A permission an installation lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// GitHub's name of it, such as `actions`.
    pub name: &'static str,
    /// The level runnerd needs.
    pub needed: Level,
    /// What the installation has.
    pub has: Granted,
}

impl App {
    /// The installation `config` names, its key read from its file now, minting from `base` (`API_URL`, or a test
    /// server).
    pub fn new(config: &GithubApp, base: &str) -> Result<App, Error> {
        let key = AppKey::load(&config.private_key)
            .map_err(|error| Error::Token(format!("the GitHub App's private key: {error}")))?;
        App::with_key(
            config.app_id,
            config.installation_id,
            key,
            base,
            REQUEST_TIMEOUT,
        )
    }

    /// The installation `installation_id` of App `app_id`, minting with `key` from `base`, each request given up
    /// after `timeout`.
    fn with_key(
        app_id: u64,
        installation_id: u64,
        key: AppKey,
        base: &str,
        timeout: Duration,
    ) -> Result<App, Error> {
        super::install_crypto_provider();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static("2022-11-28"),
        );
        let http = reqwest::Client::builder()
            .user_agent(concat!("runnerd/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| Error::Token("cannot make the GitHub App's client".to_owned()))?;
        Ok(App {
            id: app_id,
            installation: installation_id,
            key,
            http,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    /// Where its tokens come from, for `Github`'s expiring credentials: each read a mint of its own.
    pub fn source(self: Arc<App>) -> ExpiringSource {
        Arc::new(move || {
            let app = self.clone();
            async move { app.mint().await.map(|minted| minted.token) }.boxed()
        })
    }

    /// A new installation token: a JWT signed now, exchanged once, with no redirect followed, within
    /// [`REQUEST_TIMEOUT`]. How long the token lasts is read against GitHub's `Date`, so a clock that is off does not
    /// move it; without a `Date`, against this host's clock. A token that already expired is an error.
    async fn mint(&self) -> Result<Minted, Error> {
        let jwt = self.key.jwt(self.id, SystemTime::now())?;
        let path = format!("/app/installations/{}/access_tokens", self.installation);
        let response = self
            .http
            .request(Method::POST, format!("{}{path}", self.base))
            .bearer_auth(&jwt.0)
            .send()
            .await
            .map_err(|error| refused("no answer", &error))?;
        drop(jwt);
        let status = response.status();
        if status != StatusCode::CREATED && status != StatusCode::OK {
            return Err(answered(status));
        }
        let date = response
            .headers()
            .get(header::DATE)
            .and_then(|date| date.to_str().ok())
            .and_then(time::parse_http_date);
        let body = response
            .bytes()
            .await
            .map_err(|error| refused("an answer cut short", &error))?;
        let answer: AccessToken = serde_json::from_slice(&body).map_err(|_| {
            Error::Token("the GitHub App's token request: an answer that does not parse".to_owned())
        })?;
        let expires = time::parse_ms(&answer.expires_at).ok_or_else(|| {
            Error::Token("the GitHub App's token request: an expiry that does not parse".to_owned())
        })?;
        let now = time::now_ms();
        let lasts = expires.saturating_sub(date.unwrap_or(now));
        if lasts == 0 {
            return Err(Error::Token(
                "the GitHub App's token request: a token that already expired".to_owned(),
            ));
        }
        let permissions = answer
            .permissions
            .into_iter()
            .map(|(name, level)| (name, Granted::of(Some(level.as_str().unwrap_or_default()))))
            .collect();
        Ok(Minted {
            token: ExpiringToken {
                token: Token(answer.token),
                expires_at: SystemTime::now() + Duration::from_millis(lasts),
            },
            permissions,
        })
    }

    /// What the installation may do and which of `repositories` it covers, with their visibility, as the doctor
    /// checks them: a token minted for this, then every page of `GET /installation/repositories` read with it, at
    /// most 10 pages. A repository's own read is never taken for membership: an installation token may read a
    /// public repository it does not cover. Nothing written, and the token dropped when done.
    pub async fn probe(&self, repositories: &[String]) -> Result<Installation, Error> {
        let minted = self.mint().await?;
        let token = minted.token.token;
        let mut listed: BTreeMap<String, Option<Visibility>> = BTreeMap::new();
        let mut whole = false;
        for page in 1..=MAX_PAGES {
            let path = format!("/installation/repositories?per_page={PER_PAGE}&page={page}");
            let response = self
                .http
                .get(format!("{}{path}", self.base))
                .bearer_auth(&token.0)
                .send()
                .await
                .map_err(|error| listing("no answer", &error))?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Token(format!(
                    "the installation's repositories: GitHub answered {status}"
                )));
            }
            let next = super::names_next_page(response.headers());
            let body = response
                .bytes()
                .await
                .map_err(|error| listing("an answer cut short", &error))?;
            let page: Repositories = serde_json::from_slice(&body).map_err(|_| {
                Error::Token(
                    "the installation's repositories: an answer that does not parse".to_owned(),
                )
            })?;
            for repository in page.repositories {
                listed.insert(
                    repository.full_name.to_ascii_lowercase(),
                    repository.visibility.as_deref().and_then(Visibility::of),
                );
            }
            if !next {
                whole = true;
                break;
            }
        }
        if !whole {
            return Err(Error::Token(format!(
                "the installation covers more than {} repositories, which runnerd does not list",
                MAX_PAGES * PER_PAGE
            )));
        }
        let names: BTreeSet<&String> = repositories.iter().collect();
        Ok(Installation {
            permissions: REQUIRED_PERMISSIONS
                .iter()
                .map(|&(name, _)| {
                    let granted = minted.permissions.get(name).copied();
                    (name, granted.unwrap_or(Granted::Nothing))
                })
                .collect(),
            repositories: names
                .into_iter()
                .map(|name| {
                    let coverage = match listed.get(&name.to_ascii_lowercase()) {
                        Some(visibility) => Coverage::Covered(*visibility),
                        None => Coverage::Uncovered,
                    };
                    (name.clone(), coverage)
                })
                .collect(),
        })
    }
}

/// A token request that GitHub answered with `status`: no more of its answer.
fn answered(status: StatusCode) -> Error {
    Error::Token(format!(
        "the GitHub App's token request: GitHub answered {status}"
    ))
}

/// A token request that got `what` instead of an answer; whether it timed out, never the error's own words.
fn refused(what: &str, error: &reqwest::Error) -> Error {
    Error::Token(format!(
        "the GitHub App's token request: {}",
        if error.is_timeout() {
            "no answer in time"
        } else {
            what
        }
    ))
}

/// The installation's listing that got `what` instead of an answer.
fn listing(what: &str, error: &reqwest::Error) -> Error {
    Error::Token(format!(
        "the installation's repositories: {}",
        if error.is_timeout() {
            "no answer in time"
        } else {
            what
        }
    ))
}

#[cfg(test)]
mod tests;
