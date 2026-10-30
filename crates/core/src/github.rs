//! The GitHub REST API as the scheduler and the history need it: queued jobs, a job's steps, just-in-time runner
//! configs, repository runners and cancelling a run. Owns the token, the pages of a listing, conditional requests and
//! the answers kept for them, and rate-limit backoff. It must never put the token in a log line, a `Debug` output or
//! an error, nor send it anywhere but its base URL, and it decides nothing about scheduling.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::api;
use crate::docker::JitConfig;
use crate::sync::lock;
use crate::time;

/// The public API's address.
pub const API_URL: &str = "https://api.github.com";

/// How long one request may take, connecting included. GitHub answers in well under a second; a request that hangs
/// holds back the tick that made it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Items per page of a listing: GitHub's most.
const PER_PAGE: u32 = 100;

/// The most pages read of one listing: 1,000 runs, jobs of a run or runners, more than one machine serves at once
/// and four times a matrix's 256 jobs. A listing that names a page past it is cut there, so a `Link` that never ends
/// costs ten requests, not the rate limit.
const MAX_PAGES: u32 = 10;

/// How many requests for runs' jobs one poll of a repository starts: as many runs as a poll read before it followed
/// pages, 100 queued and 100 in progress. A run whose first page is read is read to its last, so none of its queued
/// jobs drops out of the listing for want of a page, and the listings of runs, however many pages they grow to, do
/// not move where a poll stops. A repository with more active runs leaves the rest unread until earlier ones end,
/// so following pages does not spend the rate limit faster than before.
const JOB_REQUESTS: usize = 200;

/// The most bytes of answers kept for conditional requests. A page of 100 runs is about 600 KB, a page of jobs with
/// their steps about 350 KB, a job's details a few KB: room for what several busy repositories ask every poll.
const CONDITIONAL_BYTES: usize = 32 * 1024 * 1024;

/// An answer asked for within this long is not dropped to make room for another. Every poll asks the same paths in
/// the same order; were the least recently used answer dropped, a poll that asks more than the limit holds would
/// find each answer dropped just before asking for it again and pay for every request. Kept like this, the answers
/// that fit stay conditional and only the rest cost the rate limit. It is far longer than a poll interval (15 s by
/// default); polls further apart than this are rare enough that what they pay matters little.
const CONDITIONAL_FRESH: Duration = Duration::from_secs(600);

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
    /// Whether people, not bots or apps, started it: GitHub's `actor` and `triggering_actor`, who re-ran it, are
    /// both of type `User`.
    pub by_user: bool,
    /// The pull requests into the repository it runs, by number. GitHub lists only those whose head branch is in
    /// the repository itself, and may list none once they are closed; those it lists into another repository of a
    /// fork network are left out.
    pub pull_requests: Vec<u64>,
}

/// A pull request, as `GET /repos/{owner}/{repo}/pulls/{number}` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// Whether a person, not a bot or an app, opened it: its `user` is of type `User`.
    pub by_user: bool,
    /// `owner/name` of the repository its head branch is in; `None` once that is deleted.
    pub head_repository: Option<String>,
    /// Its head branch.
    pub head_branch: String,
    /// The branch it is to merge into, of the repository it was opened in.
    pub base_branch: String,
}

/// A repository, as `GET /repos/{owner}/{repo}` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryDetails {
    /// Its default branch.
    pub default_branch: String,
    /// Whether it is public, private or internal.
    pub visibility: api::Visibility,
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
    /// Registers a runner for one job and returns it with the config the runner's listener takes.
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
    /// One pull request: who opened it and where its head branch is.
    fn pull_request(
        &self,
        repository: &str,
        number: u64,
    ) -> impl Future<Output = Result<PullRequest, Error>> + Send;
    /// The repository's default branch and visibility.
    fn repository(
        &self,
        repository: &str,
    ) -> impl Future<Output = Result<RepositoryDetails, Error>> + Send;
    /// Whether `branch` of the repository has the commit `sha`: its head or an ancestor of it. A tag of the same
    /// name, which git would take for it, does not count.
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
    etags: Mutex<Conditional>,
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

/// Which listing of runs a run is in; queued ones order first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RunState {
    Waiting,
    Started,
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
    actor: Option<Account>,
    triggering_actor: Option<Account>,
    #[serde(default)]
    pull_requests: Vec<PullRequestNumber>,
}

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "type")]
    kind: String,
}

impl Account {
    /// Whether GitHub says `account` is a person's: `Bot` is a GitHub App's or a bot's.
    fn is_user(account: Option<&Account>) -> bool {
        account.is_some_and(|account| account.kind == "User")
    }
}

#[derive(Deserialize)]
struct PullRequestNumber {
    number: u64,
    base: Option<PullRequestBase>,
}

#[derive(Deserialize)]
struct PullRequestBase {
    repo: Option<RepositoryUrl>,
}

#[derive(Deserialize)]
struct RepositoryUrl {
    url: String,
}

impl PullRequestNumber {
    /// Whether it is a pull request into `repository`: in a fork network, a run can also list one into another
    /// repository whose head is the same branch.
    fn targets(&self, repository: &str) -> bool {
        let Some(url) = self
            .base
            .as_ref()
            .and_then(|base| base.repo.as_ref())
            .map(|repo| repo.url.to_ascii_lowercase())
        else {
            return false;
        };
        url.ends_with(&format!("/repos/{}", repository.to_ascii_lowercase()))
    }
}

#[derive(Deserialize)]
struct PullRequestBody {
    user: Option<Account>,
    head: PullRequestHead,
    base: PullRequestBaseRef,
}

#[derive(Deserialize)]
struct PullRequestBaseRef {
    #[serde(rename = "ref")]
    branch: String,
}

#[derive(Deserialize)]
struct PullRequestHead {
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<RepositoryName>,
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
struct BranchBody {
    commit: CommitBody,
}

#[derive(Deserialize)]
struct CommitBody {
    sha: String,
}

#[derive(Deserialize)]
struct RepositoryBody {
    default_branch: String,
    visibility: api::Visibility,
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
            etags: Mutex::new(Conditional::new(CONDITIONAL_BYTES, CONDITIONAL_FRESH)),
            blocked_until: Mutex::new(None),
        })
    }

    /// When the next request may be sent: `None` now, otherwise when the rate limit resets.
    pub fn blocked_until(&self) -> Option<SystemTime> {
        let blocked = *lock(&self.blocked_until);
        blocked.filter(|until| *until > SystemTime::now())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        self.get_page(path).await.map(|page| page.body)
    }

    /// One answer, conditional on the last one kept for `path`, and whether GitHub's `Link` names a next page.
    async fn get_page<T: DeserializeOwned>(&self, path: &str) -> Result<Page<T>, Error> {
        // Taken before the request, so an answer evicted meanwhile still serves a 304.
        let cached = lock(&self.etags).get(path, Instant::now());
        let etag = cached.as_ref().map(|cached| &cached.etag);
        let response = self.send(Method::GET, path, None, etag).await?;
        let (bytes, next) = if response.status == StatusCode::NOT_MODIFIED {
            match cached {
                Some(cached) => (cached.body, cached.next),
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
            let body: Arc<[u8]> = response.bytes.into();
            if let Some(etag) = etag {
                lock(&self.etags).insert(
                    path,
                    Cached {
                        etag,
                        body: body.clone(),
                        next: response.next,
                    },
                    Instant::now(),
                );
            }
            (body, response.next)
        };
        Ok(Page {
            body: decode(path, &bytes)?,
            next,
        })
    }

    /// The pages of the listing at `path`, a query without `page`, for as long as GitHub's `Link` names a next one,
    /// at most `MAX_PAGES`. Each page is its own conditional request. The next page's address is built here, not
    /// taken from the `Link`, so the token goes nowhere but `base`. Items move between pages while they are read: one
    /// may show twice, which callers dedupe by ID, or not at all until the next poll.
    async fn get_pages<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, Error> {
        let mut pages = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = if page == 1 {
                path.to_owned()
            } else {
                format!("{path}&page={page}")
            };
            let answer: Page<T> = self.get_page(&path).await?;
            pages.push(answer.body);
            if !answer.next {
                break;
            }
        }
        Ok(pages)
    }

    /// Sends a request with the token, once more with a fresh token after a 401, conditional on `etag` if given.
    /// Records the rate limit.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        etag: Option<&HeaderValue>,
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
            if let Some(etag) = etag {
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
            let next = names_next_page(response.headers());
            let bytes = response.bytes().await.map_err(|source| Error::Http {
                method: method.clone(),
                path: path.to_owned(),
                source: source.without_url(),
            })?;
            return Ok(Response {
                status,
                etag,
                next,
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
        // Each run once, `Started` if GitHub lists it in progress, `Waiting` if queued.
        let mut runs = Vec::new();
        for (state, status) in [
            (RunState::Waiting, "queued"),
            (RunState::Started, "in_progress"),
        ] {
            let pages: Vec<Runs> = self
                .get_pages(&format!(
                    "/repos/{repository}/actions/runs?status={status}&per_page={PER_PAGE}"
                ))
                .await?;
            runs.extend(
                pages
                    .into_iter()
                    .flat_map(|page| page.workflow_runs)
                    .map(|run| (state, run.id)),
            );
        }
        // A run listed under both, which moved while they were read, counts as queued.
        runs.sort_unstable_by_key(|&(state, id)| (id, state));
        runs.dedup_by_key(|&mut (_, id)| id);
        // Queued runs first, whose jobs all wait, then those in progress, which may run on GitHub's own runners;
        // oldest first within each. Should the requests run out, the runs left unread are the newest.
        runs.sort_unstable();
        let listed: HashSet<u64> = runs.iter().map(|&(_, id)| id).collect();
        let mut jobs = Vec::new();
        let mut requests = 0;
        for &(_, run) in &runs {
            if requests >= JOB_REQUESTS {
                break;
            }
            let pages: Vec<Jobs> = self.get_pages(&jobs_path(repository, run)).await?;
            requests += pages.len();
            jobs.extend(
                pages
                    .into_iter()
                    .flat_map(|page| page.jobs)
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
        // The cached pages of runs that ended: nothing asks for them again.
        let runs_prefix = format!("/repos/{repository}/actions/runs/");
        lock(&self.etags).retain(|path| {
            match path
                .strip_prefix(&runs_prefix)
                .and_then(|rest| rest.split_once("/jobs?"))
            {
                Some((run, _)) => run.parse().is_ok_and(|run: u64| listed.contains(&run)),
                None => true,
            }
        });
        jobs.sort_by_key(|job| job.id);
        jobs.dedup_by_key(|job| job.id);
        Ok(jobs)
    }

    async fn runners(&self, repository: &str) -> Result<Vec<Runner>, Error> {
        let pages: Vec<Runners> = self
            .get_pages(&format!(
                "/repos/{repository}/actions/runners?per_page={PER_PAGE}"
            ))
            .await?;
        let mut seen = HashSet::new();
        Ok(pages
            .into_iter()
            .flat_map(|page| page.runners)
            .filter(|runner| seen.insert(runner.id))
            .collect())
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
        let response = self.send(Method::POST, &path, Some(&body), None).await?;
        let response = expect_success(Method::POST, &path, response)?;
        let jit: JitResponse = decode(&path, &response.bytes)?;
        Ok((jit.runner, JitConfig(jit.encoded_jit_config)))
    }

    async fn delete_runner(&self, repository: &str, id: u64) -> Result<Deleted, Error> {
        let path = format!("/repos/{repository}/actions/runners/{id}");
        let response = self.send(Method::DELETE, &path, None, None).await?;
        match response.status {
            StatusCode::NOT_FOUND => Ok(Deleted::NotFound),
            StatusCode::UNPROCESSABLE_ENTITY => Ok(Deleted::Busy),
            _ => expect_success(Method::DELETE, &path, response).map(|_| Deleted::Deleted),
        }
    }

    async fn job(&self, repository: &str, id: u64) -> Result<JobDetails, Error> {
        let path = format!("/repos/{repository}/actions/jobs/{id}");
        let job: JobDetails = self.get(&path).await?;
        // A completed job does not change again, and the history stops asking for it.
        if job.status == "completed" {
            lock(&self.etags).remove(&path);
        }
        Ok(job)
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
            by_user: Account::is_user(run.actor.as_ref())
                && Account::is_user(run.triggering_actor.as_ref()),
            pull_requests: run
                .pull_requests
                .into_iter()
                .filter(|pull| pull.targets(repository))
                .map(|pull| pull.number)
                .collect(),
        })
    }

    async fn pull_request(&self, repository: &str, number: u64) -> Result<PullRequest, Error> {
        let path = format!("/repos/{repository}/pulls/{number}");
        let pull: Result<PullRequestBody, Error> = self.get(&path).await;
        lock(&self.etags).remove(&path);
        let pull = pull?;
        Ok(PullRequest {
            by_user: Account::is_user(pull.user.as_ref()),
            head_repository: pull.head.repo.map(|repo| repo.full_name),
            head_branch: pull.head.branch,
            base_branch: pull.base.branch,
        })
    }

    async fn repository(&self, repository: &str) -> Result<RepositoryDetails, Error> {
        let body: RepositoryBody = self.get(&format!("/repos/{repository}")).await?;
        Ok(RepositoryDetails {
            default_branch: body.default_branch,
            visibility: body.visibility,
        })
    }

    async fn on_branch(&self, repository: &str, branch: &str, sha: &str) -> Result<bool, Error> {
        let commit =
            |sha: &str| !sha.is_empty() && sha.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !commit(sha) {
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
        // Compare would read a bare name as git does, a tag before a branch: a tag `main` would pass for the
        // branch. This endpoint knows only branches.
        let path = format!("/repos/{repository}/branches/{base}");
        let head: Result<BranchBody, Error> = self.get(&path).await;
        lock(&self.etags).remove(&path);
        let head = head?.commit.sha;
        if !commit(&head) {
            return Ok(false);
        }
        // The commits it lists are not needed: one is the fewest it sends.
        let path = format!("/repos/{repository}/compare/{head}...{sha}?per_page=1");
        let body: Result<CompareBody, Error> = self.get(&path).await;
        lock(&self.etags).remove(&path);
        // `behind`: the branch has moved on past it.
        Ok(matches!(body?.status.as_str(), "identical" | "behind"))
    }

    async fn cancel_run(&self, repository: &str, run: u64) -> Result<(), Error> {
        let path = format!("/repos/{repository}/actions/runs/{run}/cancel");
        let response = self.send(Method::POST, &path, None, None).await?;
        // 409: the run completed already.
        if response.status == StatusCode::CONFLICT {
            return Ok(());
        }
        expect_success(Method::POST, &path, response).map(|_| ())
    }
}

fn jobs_path(repository: &str, run: u64) -> String {
    format!("/repos/{repository}/actions/runs/{run}/jobs?filter=latest&per_page={PER_PAGE}")
}

struct Response {
    status: StatusCode,
    etag: Option<HeaderValue>,
    /// Whether its `Link` names a next page.
    next: bool,
    bytes: Vec<u8>,
}

/// A decoded answer and whether a next page follows it.
struct Page<T> {
    body: T,
    next: bool,
}

/// Whether a `Link` header names a next page: `<https://…&page=2>; rel="next", <…>; rel="last"`.
fn names_next_page(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|link| {
            link.split(';')
                .skip(1)
                .any(|param| param.trim() == r#"rel="next""#)
        })
}

/// An answer kept for a conditional request on its path.
#[derive(Clone)]
struct Cached {
    etag: HeaderValue,
    body: Arc<[u8]>,
    /// Whether its `Link` named a next page: a 304 does not say again.
    next: bool,
}

/// The answers kept for conditional requests, by path, within `limit` bytes. Room is made only from answers unused
/// for `fresh`, least recently used first; without enough of those, a new answer is not kept.
struct Conditional {
    entries: HashMap<String, Use>,
    bytes: usize,
    limit: usize,
    fresh: Duration,
}

struct Use {
    cached: Cached,
    used: Instant,
}

impl Conditional {
    fn new(limit: usize, fresh: Duration) -> Conditional {
        Conditional {
            entries: HashMap::new(),
            bytes: 0,
            limit,
            fresh,
        }
    }

    fn size(path: &str, cached: &Cached) -> usize {
        path.len() + cached.body.len()
    }

    /// The answer kept for `path`, which counts as using it `now`.
    fn get(&mut self, path: &str, now: Instant) -> Option<Cached> {
        let entry = self.entries.get_mut(path)?;
        entry.used = now;
        Some(entry.cached.clone())
    }

    /// Keeps `cached` for `path` in place of what was kept for it, if room can be made for it.
    fn insert(&mut self, path: &str, cached: Cached, now: Instant) {
        self.remove(path);
        let size = Conditional::size(path, &cached);
        if size > self.limit {
            return;
        }
        while self.bytes + size > self.limit {
            let stale = self
                .entries
                .iter()
                .filter(|(_, entry)| now.saturating_duration_since(entry.used) >= self.fresh)
                .min_by_key(|(_, entry)| entry.used)
                .map(|(path, _)| path.clone());
            let Some(stale) = stale else {
                return;
            };
            self.remove(&stale);
        }
        self.bytes += size;
        self.entries
            .insert(path.to_owned(), Use { cached, used: now });
    }

    fn remove(&mut self, path: &str) {
        if let Some(entry) = self.entries.remove(path) {
            self.bytes -= Conditional::size(path, &entry.cached);
        }
    }

    /// Keeps only the answers whose path `keep` accepts.
    fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        let bytes = &mut self.bytes;
        self.entries.retain(|path, entry| {
            let kept = keep(path);
            if !kept {
                *bytes -= Conditional::size(path, &entry.cached);
            }
            kept
        });
    }
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
