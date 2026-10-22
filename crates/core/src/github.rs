//! The GitHub REST API as the scheduler needs it: queued jobs, just-in-time runner configs and repository runners.
//! Owns the token, conditional requests and rate-limit backoff. It must never put the token in a log line, a
//! `Debug` output or an error, and it decides nothing about scheduling.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::docker::JitConfig;

/// The public API's address.
pub const API_URL: &str = "https://api.github.com";

/// A GitHub token. Its `Debug` output never shows the value.
#[derive(Clone)]
pub struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Where the client gets a token: at start and again when GitHub answers 401.
pub type TokenSource = Box<dyn Fn() -> Result<Token, Error> + Send + Sync>;

/// The `gh` CLI's token for github.com (`gh auth token`).
pub fn gh_token() -> Result<Token, Error> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .map_err(|error| Error::Token(format!("cannot run gh: {error}")))?;
    if !output.status.success() {
        return Err(Error::Token(format!(
            "gh auth token failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|_| Error::Token("gh auth token printed a token that is not UTF-8".to_owned()))?;
    Ok(Token(token.trim().to_owned()))
}

/// Makes ring the process's TLS crypto provider, which every `reqwest::Client` needs: reqwest is built without
/// one. Calling it again does nothing.
pub fn install_crypto_provider() {
    // Fails only when a provider is installed already.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// What a GitHub request can fail with. No variant holds the token.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// GitHub's rate limit is used up; no request is sent before `until`.
    #[error("GitHub's rate limit is used up until {}s after the epoch", secs(*until))]
    RateLimited { until: SystemTime },
    #[error("GitHub answered {status} to {method} {path}: {message}")]
    Status {
        method: Method,
        path: String,
        status: StatusCode,
        message: String,
    },
    #[error("{method} {path}: {source}")]
    Http {
        method: Method,
        path: String,
        source: reqwest::Error,
    },
    #[error("{path}: unexpected response: {source}")]
    Decode {
        path: String,
        source: serde_json::Error,
    },
    #[error("no GitHub token: {0}")]
    Token(String),
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A job waiting for a runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJob {
    pub id: u64,
    pub run_id: u64,
    pub name: String,
    /// The workflow's `runs-on` labels.
    pub labels: Vec<String>,
}

/// A runner registered on a repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    /// `online` or `offline`.
    pub status: String,
    /// Whether it runs a job.
    pub busy: bool,
}

/// What deleting a runner did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    Deleted,
    /// It was gone already: an ephemeral runner removes itself after its job.
    NotFound,
    /// GitHub refused because the runner runs a job.
    Busy,
}

/// The GitHub calls the scheduler makes. `Github` is the real client; the engine's tests use a fake.
pub trait GithubApi: Send + Sync {
    /// The repository's jobs waiting for a runner, in queued runs and in runs that started some of their jobs.
    fn queued_jobs(
        &self,
        repository: &str,
    ) -> impl Future<Output = Result<Vec<QueuedJob>, Error>> + Send;
    /// The repository's self-hosted runners.
    fn runners(&self, repository: &str) -> impl Future<Output = Result<Vec<Runner>, Error>> + Send;
    /// Registers a runner for one job and returns it with the config `run.sh --jitconfig` takes.
    fn jit_config(
        &self,
        repository: &str,
        name: &str,
        labels: &[String],
    ) -> impl Future<Output = Result<(Runner, JitConfig), Error>> + Send;
    /// Removes a runner from the repository.
    fn delete_runner(
        &self,
        repository: &str,
        id: u64,
    ) -> impl Future<Output = Result<Deleted, Error>> + Send;
}

/// A GitHub API client for one token. Requests on one URL are conditional on its last `ETag`, so an unchanged
/// answer costs no rate limit.
pub struct Github {
    http: reqwest::Client,
    base: String,
    token_source: TokenSource,
    token: Mutex<Option<Token>>,
    etags: Mutex<HashMap<String, (HeaderValue, Vec<u8>)>>,
    blocked_until: Mutex<Option<SystemTime>>,
}

impl fmt::Debug for Github {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Github").field("base", &self.base).finish()
    }
}

#[derive(Deserialize)]
struct Runs {
    workflow_runs: Vec<Run>,
}

#[derive(Deserialize)]
struct Run {
    id: u64,
}

#[derive(Deserialize)]
struct Jobs {
    jobs: Vec<Job>,
}

#[derive(Deserialize)]
struct Job {
    id: u64,
    run_id: u64,
    name: String,
    status: String,
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct Runners {
    runners: Vec<Runner>,
}

#[derive(Deserialize)]
struct JitResponse {
    runner: Runner,
    encoded_jit_config: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    message: String,
}

impl Github {
    /// A client for `base` (`API_URL`, or a test server) that takes its token from `token_source`.
    pub fn new(base: &str, token_source: TokenSource) -> Result<Github, Error> {
        install_crypto_provider();
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
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|source| Error::Http {
                method: Method::GET,
                path: base.to_owned(),
                source,
            })?;
        Ok(Github {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token_source,
            token: Mutex::new(None),
            etags: Mutex::new(HashMap::new()),
            blocked_until: Mutex::new(None),
        })
    }

    /// When the next request may be sent: `None` now, otherwise when the rate limit resets.
    pub fn blocked_until(&self) -> Option<SystemTime> {
        let blocked = *lock(&self.blocked_until);
        blocked.filter(|until| *until > SystemTime::now())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let response = self.send(Method::GET, path, None).await?;
        let bytes = if response.status == StatusCode::NOT_MODIFIED {
            let cached = lock(&self.etags).get(path).map(|(_, body)| body.clone());
            match cached {
                Some(body) => body,
                None => {
                    return Err(Error::Status {
                        method: Method::GET,
                        path: path.to_owned(),
                        status: response.status,
                        message: "not modified, but nothing is cached".to_owned(),
                    });
                }
            }
        } else {
            let etag = response.etag.clone();
            let response = expect_success(Method::GET, path, response).await?;
            if let Some(etag) = etag {
                lock(&self.etags).insert(path.to_owned(), (etag, response.bytes.clone()));
            }
            response.bytes
        };
        decode(path, &bytes)
    }

    /// Sends a request with the token, once more with a fresh token after a 401. Records the rate limit.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, Error> {
        if let Some(until) = self.blocked_until() {
            return Err(Error::RateLimited { until });
        }
        let mut refreshed = false;
        loop {
            let token = self.token(refreshed)?;
            let mut request = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .bearer_auth(&token.0);
            if let Some(body) = body {
                request = request
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.to_string());
            }
            if method == Method::GET
                && let Some((etag, _)) = lock(&self.etags).get(path)
            {
                request = request.header(header::IF_NONE_MATCH, etag.clone());
            }
            let response = request.send().await.map_err(|source| Error::Http {
                method: method.clone(),
                path: path.to_owned(),
                source: source.without_url(),
            })?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                continue;
            }
            if let Some(until) = rate_limited_until(status, response.headers(), SystemTime::now()) {
                *lock(&self.blocked_until) = Some(until);
                if matches!(
                    status,
                    StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
                ) {
                    return Err(Error::RateLimited { until });
                }
            }
            let etag = response.headers().get(header::ETAG).cloned();
            let bytes = response.bytes().await.map_err(|source| Error::Http {
                method: method.clone(),
                path: path.to_owned(),
                source: source.without_url(),
            })?;
            return Ok(Response {
                status,
                etag,
                bytes: bytes.to_vec(),
            });
        }
    }

    fn token(&self, refresh: bool) -> Result<Token, Error> {
        let mut token = lock(&self.token);
        if refresh || token.is_none() {
            *token = Some((self.token_source)()?);
        }
        token
            .clone()
            .ok_or_else(|| Error::Token("the token source returned nothing".to_owned()))
    }
}

impl GithubApi for Github {
    async fn queued_jobs(&self, repository: &str) -> Result<Vec<QueuedJob>, Error> {
        let mut jobs = Vec::new();
        let mut listed = Vec::new();
        for status in ["queued", "in_progress"] {
            let runs: Runs = self
                .get(&format!(
                    "/repos/{repository}/actions/runs?status={status}&per_page=100"
                ))
                .await?;
            for run in runs.workflow_runs {
                let path = jobs_path(repository, run.id);
                let run_jobs: Jobs = self.get(&path).await?;
                listed.push(path);
                jobs.extend(
                    run_jobs
                        .jobs
                        .into_iter()
                        .filter(|job| job.status == "queued")
                        .map(|job| QueuedJob {
                            id: job.id,
                            run_id: job.run_id,
                            name: job.name,
                            labels: job.labels,
                        }),
                );
            }
        }
        // The cached answers of runs that ended: nothing asks for them again.
        let runs_prefix = format!("/repos/{repository}/actions/runs/");
        lock(&self.etags).retain(|path, _| {
            !(path.starts_with(&runs_prefix) && path.contains("/jobs?")) || listed.contains(path)
        });
        jobs.sort_by_key(|job| job.id);
        jobs.dedup_by_key(|job| job.id);
        Ok(jobs)
    }

    async fn runners(&self, repository: &str) -> Result<Vec<Runner>, Error> {
        let runners: Runners = self
            .get(&format!("/repos/{repository}/actions/runners?per_page=100"))
            .await?;
        Ok(runners.runners)
    }

    async fn jit_config(
        &self,
        repository: &str,
        name: &str,
        labels: &[String],
    ) -> Result<(Runner, JitConfig), Error> {
        let body = serde_json::json!({
            "name": name,
            // The repository's default runner group.
            "runner_group_id": 1,
            "labels": labels,
            "work_folder": "_work",
        });
        let path = format!("/repos/{repository}/actions/runners/generate-jitconfig");
        let response = self.send(Method::POST, &path, Some(&body)).await?;
        let response = expect_success(Method::POST, &path, response).await?;
        let jit: JitResponse = decode(&path, &response.bytes)?;
        Ok((jit.runner, JitConfig(jit.encoded_jit_config)))
    }

    async fn delete_runner(&self, repository: &str, id: u64) -> Result<Deleted, Error> {
        let path = format!("/repos/{repository}/actions/runners/{id}");
        let response = self.send(Method::DELETE, &path, None).await?;
        match response.status {
            StatusCode::NOT_FOUND => Ok(Deleted::NotFound),
            StatusCode::UNPROCESSABLE_ENTITY => Ok(Deleted::Busy),
            _ => expect_success(Method::DELETE, &path, response)
                .await
                .map(|_| Deleted::Deleted),
        }
    }
}

fn jobs_path(repository: &str, run: u64) -> String {
    format!("/repos/{repository}/actions/runs/{run}/jobs?filter=latest&per_page=100")
}

struct Response {
    status: StatusCode,
    etag: Option<HeaderValue>,
    bytes: Vec<u8>,
}

async fn expect_success(method: Method, path: &str, response: Response) -> Result<Response, Error> {
    if response.status.is_success() {
        return Ok(response);
    }
    let message = serde_json::from_slice::<ErrorBody>(&response.bytes)
        .map(|body| body.message)
        .unwrap_or_else(|_| String::from_utf8_lossy(&response.bytes).into_owned());
    Err(Error::Status {
        method,
        path: path.to_owned(),
        status: response.status,
        message,
    })
}

fn decode<T: DeserializeOwned>(path: &str, bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|source| Error::Decode {
        path: path.to_owned(),
        source,
    })
}

/// When requests may resume, if this response says the rate limit is used up: the primary limit's reset time, or
/// a secondary limit's `retry-after`.
fn rate_limited_until(
    status: StatusCode,
    headers: &HeaderMap,
    now: SystemTime,
) -> Option<SystemTime> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    if number("x-ratelimit-remaining") == Some(0)
        && let Some(reset) = number("x-ratelimit-reset")
    {
        return Some(SystemTime::UNIX_EPOCH + Duration::from_secs(reset));
    }
    if matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) && let Some(seconds) = number("retry-after")
    {
        return Some(now + Duration::from_secs(seconds));
    }
    None
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panic while holding one of these locks leaves plain data behind, still usable.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
