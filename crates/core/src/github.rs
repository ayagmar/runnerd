//! The GitHub REST API as the scheduler and the history need it: queued jobs, a job's steps, just-in-time runner
//! configs, repository runners and cancelling a run. Owns the token, conditional requests and rate-limit backoff. It
//! must never put the token in a log line, a `Debug` output or an error, and it decides nothing about scheduling.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::docker::JitConfig;
use crate::sync::lock;
use crate::time;

/// The public API's address.
pub const API_URL: &str = "https://api.github.com";

/// How long one request may take, connecting included. GitHub answers in well under a second; a request that hangs
/// holds back the tick that made it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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
    RateLimited {
        /// When the limit resets.
        until: SystemTime,
    },
    /// GitHub answered with an error status.
    #[error("GitHub answered {status} to {method} {path}: {message}")]
    Status {
        /// The request's method.
        method: Method,
        /// The request's path, without the host.
        path: String,
        /// What GitHub answered.
        status: StatusCode,
        /// GitHub's error message, or the body as text.
        message: String,
    },
    /// The request did not get an answer.
    #[error("{method} {path}: {source}")]
    Http {
        /// The request's method.
        method: Method,
        /// The request's path, without the host.
        path: String,
        /// What failed, without the URL.
        source: reqwest::Error,
    },
    /// The answer is not the JSON expected.
    #[error("{path}: unexpected response: {source}")]
    Decode {
        /// The request's path, without the host.
        path: String,
        /// What did not parse.
        source: serde_json::Error,
    },
    /// No token could be read.
    #[error("no GitHub token: {0}")]
    Token(String),
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A job waiting for a runner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuedJob {
    /// The job's ID; IDs grow with time.
    pub id: u64,
    /// The workflow run it belongs to.
    pub run_id: u64,
    /// The job's name in the workflow.
    pub name: String,
    /// The workflow's `runs-on` labels.
    pub labels: Vec<String>,
    /// When GitHub queued it, in milliseconds since the epoch; 0 if GitHub's answer did not say.
    pub created_at: u64,
    /// 1 for the run's first attempt.
    pub run_attempt: u32,
    /// The workflow's name.
    pub workflow: Option<String>,
    /// The branch the run is for.
    pub branch: Option<String>,
    /// The job's page on GitHub.
    pub url: Option<String>,
}

/// A job as `GET /repos/{owner}/{repo}/actions/jobs/{id}` describes it: its progress and steps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JobDetails {
    /// The job's ID.
    pub id: u64,
    /// The workflow run it belongs to.
    pub run_id: u64,
    /// 1 for the run's first attempt.
    #[serde(default = "first_attempt")]
    pub run_attempt: u32,
    /// The job's name in the workflow.
    pub name: String,
    /// The workflow's name.
    pub workflow_name: Option<String>,
    /// The branch the run is for.
    pub head_branch: Option<String>,
    /// The job's page on GitHub.
    pub html_url: Option<String>,
    /// The workflow's `runs-on` labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// When GitHub queued it.
    pub created_at: Option<String>,
    /// `queued`, `in_progress`, `completed`, `waiting`, ….
    pub status: String,
    /// Once completed: `success`, `failure`, `cancelled`, ….
    pub conclusion: Option<String>,
    /// When the runner started it.
    pub started_at: Option<String>,
    /// When it completed.
    pub completed_at: Option<String>,
    /// The runner that took it, if one did.
    pub runner_name: Option<String>,
    /// Its steps so far.
    #[serde(default)]
    pub steps: Vec<JobStep>,
}

/// What started a workflow run, as `GET /repos/{owner}/{repo}/actions/runs/{id}` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTrigger {
    /// `push`, `pull_request`, `schedule`, `workflow_dispatch`, ….
    pub event: String,
    /// The branch it ran on: a pull request's head branch, which may be a fork's.
    pub head_branch: Option<String>,
    /// `owner/name` of the repository its commit came from: a fork's, for a pull request from one.
    pub head_repository: Option<String>,
    /// The commit it ran. A tag's run gives the tag's name as `head_branch`, so only this says whether the
    /// branch has it.
    pub head_sha: String,
}

fn first_attempt() -> u32 {
    1
}

/// A step of `JobDetails`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JobStep {
    /// GitHub's step number.
    pub number: u32,
    /// The name GitHub shows.
    pub name: String,
    /// `pending`, `queued`, `in_progress` or `completed`.
    pub status: String,
    /// Once completed: `success`, `failure`, `skipped`, ….
    pub conclusion: Option<String>,
    /// When it started.
    pub started_at: Option<String>,
    /// When it completed.
    pub completed_at: Option<String>,
}

/// A runner registered on a repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Runner {
    /// The runner's ID.
    pub id: u64,
    /// Its name; runnerd's runners start with the host's prefix.
    pub name: String,
    /// `online` or `offline`.
    pub status: String,
    /// Whether it runs a job.
    pub busy: bool,
}

/// What deleting a runner did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// GitHub removed it.
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
    /// One job's progress and steps.
    fn job(
        &self,
        repository: &str,
        id: u64,
    ) -> impl Future<Output = Result<JobDetails, Error>> + Send;
    /// What started a workflow run.
    fn run_trigger(
        &self,
        repository: &str,
        run: u64,
    ) -> impl Future<Output = Result<RunTrigger, Error>> + Send;
    /// The repository's default branch.
    fn default_branch(
        &self,
        repository: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send;
    /// Whether `branch` of the repository has the commit `sha`: its head or an ancestor of it.
    fn on_branch(
        &self,
        repository: &str,
        branch: &str,
        sha: &str,
    ) -> impl Future<Output = Result<bool, Error>> + Send;
    /// Cancels a workflow run, and so every job in it: GitHub cancels runs, not single jobs. A run that already
    /// completed is left as it is.
    fn cancel_run(
        &self,
        repository: &str,
        run: u64,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}

/// A GitHub API client for one token. Requests on one URL are conditional on its last `ETag`, so an unchanged
/// answer costs no rate limit.
pub struct Github {
    http: reqwest::Client,
    base: String,
    /// Run on Tokio's blocking pool: `gh` may wait on a keyring for seconds.
    token_source: Arc<dyn Fn() -> Result<Token, Error> + Send + Sync>,
    /// An async lock, held while the source runs: requests that need a token wait without blocking a worker, and
    /// concurrent 401s read it once.
    token: tokio::sync::Mutex<Option<Token>>,
    // Locked with `sync::lock`: a panic while holding one of these locks leaves plain data behind, still usable.
    etags: Mutex<HashMap<String, (HeaderValue, Vec<u8>)>>,
    blocked_until: Mutex<Option<SystemTime>>,
}

impl fmt::Debug for Github {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Github")
            .field("base", &self.base)
            .finish_non_exhaustive()
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
    created_at: Option<String>,
    run_attempt: Option<u32>,
    workflow_name: Option<String>,
    head_branch: Option<String>,
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct RunBody {
    event: String,
    head_branch: Option<String>,
    head_repository: Option<RepositoryName>,
    head_sha: String,
}

#[derive(Deserialize)]
struct RepositoryName {
    full_name: String,
}

#[derive(Deserialize)]
struct CompareBody {
    status: String,
}

#[derive(Deserialize)]
struct RepositoryBody {
    default_branch: String,
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
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|source| Error::Http {
                method: Method::GET,
                path: base.to_owned(),
                source,
            })?;
        Ok(Github {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token_source: Arc::from(token_source),
            token: tokio::sync::Mutex::new(None),
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
            let response = expect_success(Method::GET, path, response)?;
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
        // The token GitHub refused, once it did.
        let mut refused: Option<Token> = None;
        loop {
            let token = self.token(refused.as_ref()).await?;
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
            if status == StatusCode::UNAUTHORIZED && refused.is_none() {
                refused = Some(token);
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

    /// The token, read from the source if there is none yet or it is still the one GitHub `refused`: of several
    /// requests refused at once, the first reads a new one and the others take it.
    async fn token(&self, refused: Option<&Token>) -> Result<Token, Error> {
        let mut token = self.token.lock().await;
        let stale = match (&*token, refused) {
            (None, _) => true,
            (Some(current), Some(refused)) => current.0 == refused.0,
            (Some(_), None) => false,
        };
        if stale {
            let source = self.token_source.clone();
            *token = Some(
                tokio::task::spawn_blocking(move || source())
                    .await
                    .map_err(|error| {
                        Error::Token(format!("the token source stopped: {error}"))
                    })??,
            );
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
                            created_at: job
                                .created_at
                                .as_deref()
                                .and_then(time::parse_ms)
                                .unwrap_or(0),
                            run_attempt: job.run_attempt.unwrap_or(1),
                            workflow: job.workflow_name,
                            branch: job.head_branch,
                            url: job.html_url,
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
        let response = expect_success(Method::POST, &path, response)?;
        let jit: JitResponse = decode(&path, &response.bytes)?;
        Ok((jit.runner, JitConfig(jit.encoded_jit_config)))
    }

    async fn delete_runner(&self, repository: &str, id: u64) -> Result<Deleted, Error> {
        let path = format!("/repos/{repository}/actions/runners/{id}");
        let response = self.send(Method::DELETE, &path, None).await?;
        match response.status {
            StatusCode::NOT_FOUND => Ok(Deleted::NotFound),
            StatusCode::UNPROCESSABLE_ENTITY => Ok(Deleted::Busy),
            _ => expect_success(Method::DELETE, &path, response).map(|_| Deleted::Deleted),
        }
    }

    async fn job(&self, repository: &str, id: u64) -> Result<JobDetails, Error> {
        self.get(&format!("/repos/{repository}/actions/jobs/{id}"))
            .await
    }

    async fn run_trigger(&self, repository: &str, run: u64) -> Result<RunTrigger, Error> {
        let path = format!("/repos/{repository}/actions/runs/{run}");
        let run: Result<RunBody, Error> = self.get(&path).await;
        // Asked once per job: its large answer is not worth keeping for a conditional request.
        lock(&self.etags).remove(&path);
        let run = run?;
        Ok(RunTrigger {
            event: run.event,
            head_branch: run.head_branch,
            head_repository: run.head_repository.map(|head| head.full_name),
            head_sha: run.head_sha,
        })
    }

    async fn default_branch(&self, repository: &str) -> Result<String, Error> {
        let body: RepositoryBody = self.get(&format!("/repos/{repository}")).await?;
        Ok(body.default_branch)
    }

    async fn on_branch(&self, repository: &str, branch: &str, sha: &str) -> Result<bool, Error> {
        if sha.is_empty() || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(false);
        }
        // A branch name may hold `#`, `%` or `?`, which would end or change the path.
        let base: String = branch
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.') {
                    char::from(byte).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect();
        // The commits it lists are not needed: one is the fewest it sends.
        let path = format!("/repos/{repository}/compare/{base}...{sha}?per_page=1");
        let body: Result<CompareBody, Error> = self.get(&path).await;
        lock(&self.etags).remove(&path);
        // `behind`: the branch has moved on past it.
        Ok(matches!(body?.status.as_str(), "identical" | "behind"))
    }

    async fn cancel_run(&self, repository: &str, run: u64) -> Result<(), Error> {
        let path = format!("/repos/{repository}/actions/runs/{run}/cancel");
        let response = self.send(Method::POST, &path, None).await?;
        // 409: the run completed already.
        if response.status == StatusCode::CONFLICT {
            return Ok(());
        }
        expect_success(Method::POST, &path, response).map(|_| ())
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

fn expect_success(method: Method, path: &str, response: Response) -> Result<Response, Error> {
    if response.status.is_success() {
        return Ok(response);
    }
    let message = serde_json::from_slice::<ErrorBody>(&response.bytes).map_or_else(
        |_| String::from_utf8_lossy(&response.bytes).into_owned(),
        |body| body.message,
    );
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

#[cfg(test)]
mod tests;
