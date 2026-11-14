//! The GitHub REST API as the scheduler and the history need it: queued jobs, a job's steps, just-in-time runner
//! configs, repository runners, cancelling a run, and the owner's job actions (ADR 0134). Owns the token (`token`),
//! which workflow each run is of and what it is named (`workflows`, ADR 0140),
//! the pages of a listing, conditional requests and the answers kept for them, rate-limit backoff, and the count of
//! what it asks and the limit it last read. It must never put the token in a log line, a `Debug` output or an error,
//! nor send it anywhere but its base URL, and it decides nothing about scheduling. A job action's request is read for
//! its status and headers only: its body is never read, and what it returns is a class and a status, never text.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures_util::future::BoxFuture;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::info;

use self::token::Tokens;
use self::workflows::{Next, Workflows};
use crate::api;
use crate::command;
use crate::docker::JitConfig;
use crate::sync::lock;
use crate::time;

mod requests;
mod token;
mod workflows;

use requests::Counter;
pub use requests::{Class, RateLimit, Report, Spend, Tally};

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

/// How long `gh auth token` may take, a keyring's prompt included, before its process group is killed: under a job
/// action's deadline, so the action can still say what happened.
pub const TOKEN_DEADLINE: Duration = Duration::from_secs(15);

/// The `gh` CLI's token for github.com (`gh auth token`), in a process group of its own killed past
/// [`TOKEN_DEADLINE`] or once the daemon stops (`command::RUNNING`).
pub fn gh_token() -> Result<Token, Error> {
    let output = command::output_within(
        std::process::Command::new("gh").args(["auth", "token", "--hostname", "github.com"]),
        TOKEN_DEADLINE,
        &command::RUNNING,
    )
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

impl Error {
    /// Whether GitHub answered that what was asked is not there for this token: deleted, or the token cannot see it.
    /// Asking again gets the same answer.
    pub fn is_gone(&self) -> bool {
        matches!(
            self,
            Error::Status {
                status: StatusCode::NOT_FOUND | StatusCode::GONE,
                ..
            }
        )
    }
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
    /// The workflow's name, as its file names it: the same for every run, whatever the run's `run-name`.
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
    /// The workflow's name, as its file names it: the same for every run, whatever the run's `run-name`. GitHub's
    /// answer gives the run's name here; [`Github::job`] puts the workflow's in its place.
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

/// A run's workflow name, learnt after its jobs were read under GitHub's name for the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRun {
    /// The repository, `owner/name`.
    pub repository: String,
    /// The run.
    pub run_id: u64,
    /// Its workflow's name.
    pub workflow: String,
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

/// A write the owner asks for through `POST /v1/jobs/{id}/cancel` and `POST /v1/jobs/{id}/rerun` (ADR 0134).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobActionRequest {
    /// `POST /repos/{owner}/{repo}/actions/runs/{run}/cancel`: the whole run.
    CancelRun {
        /// The run.
        run: u64,
    },
    /// `POST /repos/{owner}/{repo}/actions/jobs/{job}/rerun`: the job and the jobs that depend on it.
    RerunJob {
        /// The job.
        job: u64,
    },
    /// `POST /repos/{owner}/{repo}/actions/runs/{run}/rerun-failed-jobs`: the run's failed jobs and their dependents.
    RerunFailed {
        /// The run.
        run: u64,
    },
}

impl JobActionRequest {
    fn path(self, repository: &str) -> String {
        match self {
            JobActionRequest::CancelRun { run } => {
                format!("/repos/{repository}/actions/runs/{run}/cancel")
            }
            JobActionRequest::RerunJob { job } => {
                format!("/repos/{repository}/actions/jobs/{job}/rerun")
            }
            JobActionRequest::RerunFailed { run } => {
                format!("/repos/{repository}/actions/runs/{run}/rerun-failed-jobs")
            }
        }
    }
}

/// A workflow run as a job action needs it: which attempt is current, and whether it completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunNow {
    /// Its current attempt, from 1.
    pub attempt: u32,
    /// Whether GitHub says it completed.
    pub completed: bool,
}

/// Why a job action's request, or its read of the run, got no success: a class and a status. Holds no text, so
/// nothing GitHub wrote, and no token, can leave through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionFailure {
    /// No token could be read: nothing was sent.
    NoToken,
    /// GitHub's rate limit: `status` is GitHub's answer (403 or 429), `None` when runnerd held the request back
    /// itself, sending nothing.
    RateLimited {
        /// The HTTP status GitHub answered with, if it did.
        status: Option<u16>,
        /// When requests may resume.
        until: SystemTime,
    },
    /// GitHub answered with this status.
    Status {
        /// The HTTP status.
        status: u16,
    },
    /// The connection could not be made: nothing was sent.
    Unreachable,
    /// No answer in time; the request may have been sent.
    TimedOut,
    /// The connection failed otherwise; the request may have been sent.
    Broken,
    /// The caller's permit said no once the token was in hand: nothing was sent.
    Withdrawn,
}

/// Asked once a job action's token is in hand and before each send of it, the one after a 401 included: whether it
/// may still go. It may read GitHub itself; nothing is sent while it runs.
pub type Permit<'permit> = dyn Fn() -> BoxFuture<'permit, bool> + Send + Sync + 'permit;

impl ActionFailure {
    /// The class and status of `error`, its text dropped.
    fn of(error: &Error) -> ActionFailure {
        match error {
            Error::Token(_) => ActionFailure::NoToken,
            Error::RateLimited { until } => ActionFailure::RateLimited {
                status: None,
                until: *until,
            },
            Error::Status { status, .. } => ActionFailure::Status {
                status: status.as_u16(),
            },
            Error::Http { source, .. } if source.is_connect() => ActionFailure::Unreachable,
            Error::Http { source, .. } if source.is_timeout() => ActionFailure::TimedOut,
            Error::Http { .. } | Error::Decode { .. } => ActionFailure::Broken,
        }
    }
}

/// A permit that said no: [`Github::exchange`] sent nothing.
struct Withdrawn;

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
    /// Sends an owner's job action once, and returns GitHub's success status (201, 202, …) without reading its body,
    /// which is an acknowledgement, not a new job. It is never sent again but after a 401, which GitHub answers before
    /// it acts. `permit` is asked once the token is in hand, before each send, the 401's second included: when it
    /// says no, nothing is sent and the answer is [`ActionFailure::Withdrawn`].
    fn job_action(
        &self,
        repository: &str,
        request: JobActionRequest,
        permit: &Permit<'_>,
    ) -> impl Future<Output = Result<u16, ActionFailure>> + Send;
    /// Reads a workflow run's current attempt and whether it completed, for a job action; failures as a class and a
    /// status.
    fn action_run(
        &self,
        repository: &str,
        run: u64,
    ) -> impl Future<Output = Result<RunNow, ActionFailure>> + Send;
    /// The workflows' names of the runs whose job reads kept GitHub's name because the workflow's could not be read
    /// then, as far as they can be read now (ADR 0140). Each run is given once; one whose workflow GitHub does not
    /// give is dropped, and keeps GitHub's name.
    fn named_runs(&self) -> impl Future<Output = Vec<NamedRun>> + Send;
    /// What the reads are paced by (ADR 0127): the rate limit as last read, and the registrations and deletes sent.
    fn spend(&self) -> Spend;
}

/// A GitHub API client for one token. Requests on one URL are conditional on its last `ETag`, so an unchanged
/// answer costs no rate limit.
pub struct Github {
    http: reqwest::Client,
    base: String,
    /// The token, read on a thread of its own: `gh` may wait on a keyring for seconds.
    tokens: Tokens,
    // Locked with `sync::lock`: a panic while holding one of these locks leaves plain data behind, still usable.
    etags: Mutex<Conditional>,
    workflows: Mutex<Workflows>,
    /// Held while a run or the workflows are read for a workflow's name, so concurrent job reads ask once.
    resolving: tokio::sync::Mutex<()>,
    blocked_until: Mutex<Option<SystemTime>>,
    /// What was asked of GitHub: logged hourly and when the client goes.
    requests: Mutex<Counter>,
}

impl Drop for Github {
    /// The last window's line, as the daemon stops.
    fn drop(&mut self) {
        let report = lock(&self.requests).take(Instant::now());
        if !report.is_empty() {
            info!("{}", report.line(SystemTime::now()));
        }
    }
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
    workflow_id: Option<u64>,
}

/// A run as far as its workflow: `GET /repos/{owner}/{repo}/actions/runs/{id}`.
#[derive(Deserialize)]
struct RunWorkflow {
    workflow_id: Option<u64>,
}

#[derive(Deserialize)]
struct WorkflowList {
    workflows: Vec<WorkflowBody>,
}

#[derive(Deserialize)]
struct WorkflowBody {
    id: u64,
    name: Option<String>,
}

/// A workflow's name that could not be read: a read failed, now or within a minute.
struct Unresolved;

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
    workflow_id: Option<u64>,
    event: String,
    head_branch: Option<String>,
    head_repository: Option<RepositoryName>,
    head_sha: String,
    actor: Option<Account>,
    triggering_actor: Option<Account>,
    #[serde(default)]
    pull_requests: Vec<PullRequestNumber>,
}

/// A run's attempt and status, as a job action reads them.
#[derive(Deserialize)]
struct RunAttemptBody {
    #[serde(default = "first_attempt")]
    run_attempt: u32,
    status: Option<String>,
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
            tokens: Tokens::new(Arc::from(token_source)),
            etags: Mutex::new(Conditional::new(CONDITIONAL_BYTES, CONDITIONAL_FRESH)),
            workflows: Mutex::new(Workflows::default()),
            resolving: tokio::sync::Mutex::new(()),
            blocked_until: Mutex::new(None),
            requests: Mutex::new(Counter::new(Instant::now(), requests::WINDOW)),
        })
    }

    /// What the requests of this client counted since its last line was logged, and the rate limit as last read.
    pub fn requests(&self) -> Report {
        lock(&self.requests).peek(Instant::now())
    }

    /// Counts a response, and logs the window's line if its hour is over.
    fn count(&self, class: Class, status: StatusCode, headers: &HeaderMap) {
        let due = {
            let mut requests = lock(&self.requests);
            requests.record(class, status, headers);
            requests.due(Instant::now())
        };
        if let Some(report) = due {
            info!("{}", report.line(SystemTime::now()));
        }
    }

    /// When the next request may be sent: `None` now, otherwise when the rate limit resets.
    pub fn blocked_until(&self) -> Option<SystemTime> {
        let blocked = *lock(&self.blocked_until);
        blocked.filter(|until| *until > SystemTime::now())
    }

    /// The name of the workflow `repository`'s run `run` is of, as its file names it, which GitHub's `workflow_name`
    /// is not: that is the run's name, which a `run-name` changes on every run (ADR 0140). Reads the run once, unless a
    /// listing gave its workflow, and the repository's workflows once an hour, and once more for a workflow the last
    /// read lacked; one such read at a time, and none for a repository within a minute of one failing. `Ok(None)` when
    /// GitHub gives the run no workflow, no longer has the run, or does not list its workflow; `Err` when a read
    /// failed, or failed lately.
    async fn workflow_name(
        &self,
        repository: &str,
        run: u64,
    ) -> Result<Option<String>, Unresolved> {
        // A name already learnt waits for no read in flight.
        match lock(&self.workflows).next(repository, run, Instant::now()) {
            Next::Named(name) => return Ok(Some(name)),
            Next::Unnamed => return Ok(None),
            Next::Waiting => return Err(Unresolved),
            Next::ReadRun | Next::ReadList(_) => {}
        }
        let _reading = self.resolving.lock().await;
        // Each pass reads at most once: the run, then the workflows, whose read answers.
        for _ in 0..3 {
            let next = lock(&self.workflows).next(repository, run, Instant::now());
            match next {
                Next::Named(name) => return Ok(Some(name)),
                Next::Unnamed => return Ok(None),
                Next::Waiting => return Err(Unresolved),
                Next::ReadRun => {
                    let path = format!("/repos/{repository}/actions/runs/{run}");
                    let body: Result<RunWorkflow, Error> = self.get(&path).await;
                    // Its workflow never changes: nothing asks for the run again.
                    lock(&self.etags).remove(&path);
                    let mut workflows = lock(&self.workflows);
                    match body {
                        Ok(body) => workflows.run(repository, run, body.workflow_id),
                        Err(error) if error.is_gone() => workflows.run(repository, run, None),
                        Err(_) => workflows.failed(repository, Instant::now()),
                    }
                }
                Next::ReadList(workflow) => {
                    // Kept for a conditional request: the hourly read of an unchanged list is a 304.
                    let path = format!("/repos/{repository}/actions/workflows?per_page={PER_PAGE}");
                    let pages: Result<Vec<WorkflowList>, Error> = self.get_pages(&path).await;
                    let mut workflows = lock(&self.workflows);
                    match pages {
                        Ok(pages) => {
                            let names = pages
                                .into_iter()
                                .flat_map(|page| page.workflows)
                                .filter_map(|listed| Some((listed.id, listed.name?)))
                                .collect();
                            // This read answers, however old a list of names may be.
                            return Ok(workflows.listed(
                                repository,
                                names,
                                workflow,
                                Instant::now(),
                            ));
                        }
                        Err(_) => workflows.failed(repository, Instant::now()),
                    }
                }
            }
        }
        Err(Unresolved)
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

    /// Sends a request with the token, once more with a fresh token after a 401, conditional on `etag` if given, and
    /// reads its answer. Records the rate limit.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        etag: Option<&HeaderValue>,
    ) -> Result<Response, Error> {
        let Ok(answered) = self
            .exchange(&method, path, body, etag, || async {
                Ok::<(), std::convert::Infallible>(())
            })
            .await?;
        let Answered { response, limited } = answered;
        let status = response.status();
        if let Some(until) = limited
            && matches!(
                status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(Error::RateLimited { until });
        }
        let etag = response.headers().get(header::ETAG).cloned();
        let next = names_next_page(response.headers());
        let bytes = response.bytes().await.map_err(|source| Error::Http {
            method: method.clone(),
            path: path.to_owned(),
            source: source.without_url(),
        })?;
        Ok(Response {
            status,
            etag,
            next,
            bytes: bytes.to_vec(),
        })
    }

    /// Sends a request with the token, once more with a fresh token after a 401, and gives its answer unread.
    /// `permit` is asked once the token is in hand, before each send: when it says no, nothing is sent. Records the
    /// rate limit, and says when it ends if the answer says it is used up.
    async fn exchange<W, Allowed: Future<Output = Result<(), W>>>(
        &self,
        method: &Method,
        path: &str,
        body: Option<&serde_json::Value>,
        etag: Option<&HeaderValue>,
        permit: impl Fn() -> Allowed,
    ) -> Result<Result<Answered, W>, Error> {
        if let Some(until) = self.blocked_until() {
            return Err(Error::RateLimited { until });
        }
        let class = Class::of(method, path);
        // The token GitHub refused, once it did.
        let mut refused: Option<Token> = None;
        loop {
            let token = self.tokens.get(refused.as_ref()).await?;
            if let Err(withdrawn) = permit().await {
                return Ok(Err(withdrawn));
            }
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
            self.count(class, status, response.headers());
            if status == StatusCode::UNAUTHORIZED && refused.is_none() {
                refused = Some(token);
                continue;
            }
            let limited = rate_limited_until(status, response.headers(), SystemTime::now());
            if let Some(until) = limited {
                *lock(&self.blocked_until) = Some(until);
            }
            return Ok(Ok(Answered { response, limited }));
        }
    }
}

/// An answer as it came, its body unread, and when the rate limit it says is used up resets.
struct Answered {
    response: reqwest::Response,
    limited: Option<SystemTime>,
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
            let listed = pages.into_iter().flat_map(|page| page.workflow_runs);
            let mut workflows = lock(&self.workflows);
            runs.extend(listed.map(|run| {
                if let Some(workflow) = run.workflow_id {
                    workflows.run(repository, run.id, Some(workflow));
                }
                (state, run.id)
            }));
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
            let queued: Vec<Job> = pages
                .into_iter()
                .flat_map(|page| page.jobs)
                .filter(|job| job.status == "queued")
                .collect();
            let Some(first) = queued.first() else {
                continue;
            };
            // GitHub's name for a listing that cannot read the workflow's: the job is listed all the same, and its
            // details name it once they are read.
            let workflow = match self.workflow_name(repository, first.run_id).await {
                Ok(Some(name)) => Some(name),
                Ok(None) | Err(Unresolved) => first.workflow_name.clone(),
            };
            jobs.extend(queued.into_iter().map(|job| {
                QueuedJob {
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
                    workflow: workflow.clone(),
                    branch: job.head_branch,
                    url: job.html_url,
                }
            }));
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
        let mut job: JobDetails = self.get(&path).await?;
        // A completed job does not change again, and the history stops asking for it.
        if job.status == "completed" {
            lock(&self.etags).remove(&path);
        }
        // A job read never fails for its workflow's name: one that cannot be read keeps GitHub's, and the run is owed
        // the workflow's, which `named_runs` gives once it can be read. A completed job's details are written once.
        match self.workflow_name(repository, job.run_id).await {
            Ok(Some(name)) => job.workflow_name = Some(name),
            Ok(None) => {}
            Err(Unresolved) => lock(&self.workflows).owe(repository, job.run_id),
        }
        Ok(job)
    }

    async fn run_trigger(&self, repository: &str, run: u64) -> Result<RunTrigger, Error> {
        let run_id = run;
        let path = format!("/repos/{repository}/actions/runs/{run}");
        let run: Result<RunBody, Error> = self.get(&path).await;
        // Asked once per job: its large answer is not worth keeping for a conditional request.
        lock(&self.etags).remove(&path);
        let run = run?;
        if let Some(workflow) = run.workflow_id {
            lock(&self.workflows).run(repository, run_id, Some(workflow));
        }
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

    async fn job_action(
        &self,
        repository: &str,
        request: JobActionRequest,
        permit: &Permit<'_>,
    ) -> Result<u16, ActionFailure> {
        let path = request.path(repository);
        let allowed = || async {
            if permit().await {
                Ok(())
            } else {
                Err(Withdrawn)
            }
        };
        let answered = match self
            .exchange(&Method::POST, &path, None, None, allowed)
            .await
        {
            Ok(Ok(answered)) => answered,
            Ok(Err(Withdrawn)) => return Err(ActionFailure::Withdrawn),
            Err(error) => return Err(ActionFailure::of(&error)),
        };
        // The body is dropped unread: a 202 is the acknowledgement, whatever follows it or does not.
        let status = answered.response.status();
        if let Some(until) = answered.limited
            && matches!(
                status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(ActionFailure::RateLimited {
                status: Some(status.as_u16()),
                until,
            });
        }
        if status.is_success() {
            Ok(status.as_u16())
        } else {
            Err(ActionFailure::Status {
                status: status.as_u16(),
            })
        }
    }

    async fn action_run(&self, repository: &str, run: u64) -> Result<RunNow, ActionFailure> {
        let body: RunAttemptBody = self
            .get(&format!("/repos/{repository}/actions/runs/{run}"))
            .await
            .map_err(|error| ActionFailure::of(&error))?;
        Ok(RunNow {
            attempt: body.run_attempt,
            completed: body.status.as_deref() == Some("completed"),
        })
    }

    async fn named_runs(&self) -> Vec<NamedRun> {
        let owed = lock(&self.workflows).owed();
        let mut named = Vec::new();
        for (repository, runs) in owed {
            for run in runs {
                match self.workflow_name(&repository, run).await {
                    Ok(Some(workflow)) => named.push(NamedRun {
                        repository: repository.clone(),
                        run_id: run,
                        workflow,
                    }),
                    Ok(None) => {}
                    // The repository's reads wait a minute: its other runs too.
                    Err(Unresolved) => break,
                }
                lock(&self.workflows).paid(&repository, run);
            }
        }
        named
    }

    fn spend(&self) -> Spend {
        lock(&self.requests).spend()
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

/// The number a response header carries, if it has the header and it is one.
fn header_number(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

/// When requests may resume, if this response says the rate limit is used up: the primary limit's reset time, or
/// a secondary limit's `retry-after`.
fn rate_limited_until(
    status: StatusCode,
    headers: &HeaderMap,
    now: SystemTime,
) -> Option<SystemTime> {
    let number = |name: &str| header_number(headers, name);
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
