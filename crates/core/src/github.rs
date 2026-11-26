//! The GitHub REST API as the scheduler and the history need it: queued jobs, a job's steps, just-in-time runner
//! configs, repository runners, cancelling a run, and the owner's job actions (ADR 0134). Owns the token (`token`, a
//! command's or an expiring one, ADR 0181), the anonymous reads of a pinned action's repository (`public`),
//! which workflow each run is of and what it is named (`workflows`, ADR 0140),
//! the pages of a listing, conditional requests and the answers kept for them, rate-limit backoff, and the count of
//! what it asks and the limit it last read. It must never put the token in a log line, a `Debug` output or an error,
//! nor send it anywhere but its base URL, and it decides nothing about scheduling. It also reads what the action
//! archives need (`archives`, ADR 0147). A job action's request is read for
//! its status and headers only: its body is never read, and what it returns is a class and a status, never text.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures_util::future::BoxFuture;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::info;

use self::public::Anonymous;
use self::token::Tokens;
use self::workflows::{Next, Workflows};
use crate::api;
use crate::command;
use crate::docker::JitConfig;
use crate::sync::lock;
use crate::time;

pub mod app;
mod archives;
mod public;
mod requests;
mod token;
mod workflows;

use requests::Counter;

tokio::task_local! {
    /// The requests sent to GitHub's API within a scope, each send counted, a send again with a new token after a 401
    /// included: what the archives task's refresh is held to (ADR 0147). Outside a scope, nothing counts.
    pub static REQUESTS: std::cell::Cell<u64>;
}

/// How many requests went to GitHub's API in this task's `REQUESTS` scope; 0 outside one.
pub fn requests_sent() -> u64 {
    REQUESTS.try_with(std::cell::Cell::get).unwrap_or(0)
}
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

impl Token {
    /// A token of `value`, for a test's fake source.
    #[cfg(test)]
    pub(crate) fn from_test(value: &str) -> Token {
        Token(value.to_owned())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Where the client gets a token: at start and again when GitHub answers 401.
pub type TokenSource = Box<dyn Fn() -> Result<Token, Error> + Send + Sync>;

/// A token that GitHub stops taking at `expires_at`, as a GitHub App installation's. Its `Debug` output never shows
/// the token.
#[derive(Debug, Clone)]
pub struct ExpiringToken {
    /// The token.
    pub token: Token,
    /// When GitHub stops taking it.
    pub expires_at: SystemTime,
}

/// Where the client gets a token that expires: a request of its own, run as a task on the runtime and given up after
/// [`TOKEN_DEADLINE`]. Asked at start, shortly before the token expires, and when GitHub answers 401; after a failed
/// read, not again before a backoff. Its error must not hold the token nor anything that mints one.
pub type ExpiringSource =
    Arc<dyn Fn() -> BoxFuture<'static, Result<ExpiringToken, Error>> + Send + Sync>;

/// What the client authenticates with (ADR 0181).
pub enum Credentials {
    /// A command's token, kept until GitHub refuses it: [`gh_token`]. Read on a thread of its own.
    Command(TokenSource),
    /// A token that expires, kept until shortly before it does or until GitHub refuses it.
    Expiring(ExpiringSource),
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Credentials::Command(_) => "Credentials::Command",
            Credentials::Expiring(_) => "Credentials::Expiring",
        })
    }
}

/// How the action archives read a pinned action's repository, which is another's (ADR 0181).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicReads {
    /// With the client's token, as every other read: a `gh` user's token sees every public repository.
    Authenticated,
    /// Without any credential, on a budget, a backoff and answers kept of their own: a token scoped to some
    /// repositories may not say whether another is public.
    Anonymous,
}

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
        /// GitHub's error message, or the body as text, with anything shaped like a credential redacted (`scrub`).
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
    #[error("{path}: unexpected response: {message}")]
    Decode {
        /// The request's path, without the host.
        path: String,
        /// What did not parse, as the parser says it, with anything shaped like a credential redacted (`scrub`).
        message: String,
    },
    /// No token could be read.
    #[error("no GitHub token: {0}")]
    Token(String),
    /// An action archive, a file or a listing could not be read whole: what failed, never an address or the token.
    #[error("{0}")]
    Download(String),
    /// Not sent: runnerd held it back, to keep GitHub's rate limit for what runners need.
    #[error("not sent: {0}")]
    Held(&'static str),
}

impl Error {
    /// What failed, without GitHub's own words or anything it sent back: for a log line about action archives, whose
    /// repositories are not runnerd's.
    pub fn brief(&self) -> String {
        match self {
            Error::Status { status, path, .. } => format!("GitHub answered {status} to {path}"),
            Error::Http { path, .. } => format!("{path}: no answer"),
            Error::Decode { path, .. } => format!("{path}: an answer that does not parse"),
            other => other.to_string(),
        }
    }

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

/// What one listing of a repository's queued jobs read (ADR 0180): the jobs waiting for a runner, and the followed jobs
/// the same run pages showed in any other status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobListing {
    /// The jobs waiting for a runner, by ID.
    pub queued: Vec<QueuedJob>,
    /// Those of the jobs the listing was asked to follow that a page it read showed and that were not queued, each as
    /// [`GithubApi::job`] would describe it then: its run's latest attempt, as the pages are read with
    /// `filter=latest`, its workflow named as a job read names it. A followed job is left out when no page read
    /// showed it (its run ended, was left unread, or GitHub moved it between pages), when it was of an earlier
    /// attempt, when its details lack what its status implies (a runner and a start once in progress, a conclusion
    /// and an end once completed), and when its workflow's name would take a read the listing did not make. Its
    /// absence says nothing of the job: only a read of it does.
    pub observed: Vec<JobDetails>,
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
    /// Its labels' names, which GitHub lists as objects with a `name`; none where an answer left them out.
    #[serde(default, deserialize_with = "label_names")]
    pub labels: Vec<String>,
}

/// GitHub's `labels` of a runner, `[{ "id": 1, "name": "self-hosted", "type": "read-only" }]`, as their names.
fn label_names<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    /// One label as GitHub lists it.
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }
    Ok(Vec::<Label>::deserialize(deserializer)?
        .into_iter()
        .map(|label| label.name)
        .collect())
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
    /// Whether it completed as `failure`: not cancelled, not timed out as a whole, not anything else.
    pub failed: bool,
}

/// A job of one attempt of a workflow run, as a re-run of a run its runner failed reads it (ADR 0153).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AttemptJob {
    /// The job's ID.
    pub id: u64,
    /// `queued`, `in_progress`, `completed`, ….
    pub status: String,
    /// Once completed: `success`, `failure`, `cancelled`, `skipped`, `timed_out`, ….
    pub conclusion: Option<String>,
}

/// The jobs of an attempt, as GitHub lists them.
#[derive(Deserialize)]
struct AttemptJobs {
    /// How many jobs the attempt has, every page's. Required: a listing without it cannot be known whole.
    total_count: Option<usize>,
    jobs: Vec<AttemptJob>,
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
            Error::Http { .. } | Error::Decode { .. } | Error::Download(_) | Error::Held(_) => {
                ActionFailure::Broken
            }
        }
    }
}

/// A permit that said no: [`Github::exchange`] sent nothing.
struct Withdrawn;

/// The GitHub calls the scheduler makes. `Github` is the real client; the engine's tests use a fake.
pub trait GithubApi: crate::archives::Source + Send + Sync {
    /// The repository's jobs waiting for a runner, in queued runs and in runs that started some of their jobs, and of
    /// the jobs in `following`, those the same pages showed in another status (ADR 0180): no request more is made for
    /// them.
    fn queued_jobs(
        &self,
        repository: &str,
        following: &BTreeSet<u64>,
    ) -> impl Future<Output = Result<JobListing, Error>> + Send;
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
    /// The jobs of attempt `attempt` of a workflow run, every runner's, with their conclusions, for a re-run of a run
    /// its runner failed (ADR 0153); failures as a class and a status.
    fn attempt_jobs(
        &self,
        repository: &str,
        run: u64,
        attempt: u32,
    ) -> impl Future<Output = Result<Vec<AttemptJob>, ActionFailure>> + Send;
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
    /// The action archives' downloads (ADR 0147): anonymous, no redirect followed, a longer timeout.
    downloads: reqwest::Client,
    base: String,
    /// Where the action archives are downloaded from, anonymously: codeload.github.com, or a test server.
    codeload: String,
    /// The token, read on a thread of its own (`gh` may wait on a keyring for seconds), or minted by a task.
    tokens: Tokens,
    /// How a pinned action's repository is read: with the token, or by a client of its own that has none.
    public: Public,
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

/// A page of a run's jobs. Each item is read twice: as the listing's `Job`, which must parse, and, for a followed job,
/// as the `JobDetails` a job read gives, which may not.
#[derive(Deserialize)]
struct Jobs {
    jobs: Vec<serde_json::Value>,
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
    conclusion: Option<String>,
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

/// How the client reads a pinned action's repository: [`PublicReads`], with the anonymous client when it has one.
enum Public {
    Authenticated,
    Anonymous(Anonymous),
}

impl Github {
    /// A client for `base` (`API_URL`, or a test server) that takes its token from `token_source` and reads pinned
    /// actions' repositories with it.
    pub fn new(base: &str, token_source: TokenSource) -> Result<Github, Error> {
        Github::with_credentials(
            base,
            Credentials::Command(token_source),
            PublicReads::Authenticated,
        )
    }

    /// A client for `base` that authenticates as the GitHub App installation `app` (ADR 0182), with its expiring
    /// tokens, and reads pinned actions' repositories anonymously: an installation's token may not say whether a
    /// repository it does not cover is public.
    pub fn new_app(base: &str, app: Arc<app::App>) -> Result<Github, Error> {
        Github::with_credentials(
            base,
            Credentials::Expiring(app.source()),
            PublicReads::Anonymous,
        )
    }

    /// Reads a token now and keeps it, as the first request would: the daemon's start fails here rather than at its
    /// first poll when no token can be had.
    pub async fn authenticate(&self) -> Result<(), Error> {
        self.tokens.get(None).await.map(|_| ())
    }

    /// A client for `base` that authenticates with `credentials` and reads pinned actions' repositories as `public`
    /// says, anonymously from `base` too.
    pub fn with_credentials(
        base: &str,
        credentials: Credentials,
        public: PublicReads,
    ) -> Result<Github, Error> {
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
        let downloads = reqwest::Client::builder()
            .user_agent(concat!("runnerd/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(archives::DOWNLOAD_TIMEOUT)
            .build()
            .map_err(|source| Error::Http {
                method: Method::GET,
                path: base.to_owned(),
                source,
            })?;
        Ok(Github {
            http,
            downloads,
            base: base.trim_end_matches('/').to_owned(),
            codeload: archives::CODELOAD.to_owned(),
            tokens: Tokens::new(match credentials {
                Credentials::Command(source) => token::Source::Command(Arc::from(source)),
                Credentials::Expiring(source) => token::Source::Expiring(source),
            }),
            public: match public {
                PublicReads::Authenticated => Public::Authenticated,
                PublicReads::Anonymous => Public::Anonymous(Anonymous::new(base)?),
            },
            etags: Mutex::new(Conditional::new(CONDITIONAL_BYTES, CONDITIONAL_FRESH)),
            workflows: Mutex::new(Workflows::default()),
            resolving: tokio::sync::Mutex::new(()),
            blocked_until: Mutex::new(None),
            requests: Mutex::new(Counter::new(Instant::now(), requests::WINDOW)),
        })
    }

    /// This client with the action archives downloaded from `url` in place of codeload.github.com.
    #[cfg(test)]
    pub(crate) fn with_codeload(mut self, url: &str) -> Github {
        url.trim_end_matches('/').clone_into(&mut self.codeload);
        self
    }

    /// This client with its token's expiry checked by `clock`.
    #[cfg(test)]
    fn with_token_clock(mut self, clock: token::Clock) -> Github {
        self.tokens.set_clock(clock);
        self
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
                    // Kept for a conditional request: the build cache asks for the same run (ADR 0179).
                    let body: Result<RunWorkflow, Error> = self.get(&path).await;
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

    /// The jobs of `repository`'s run `run`, its latest attempt's: those queued, and of those in `following`, the
    /// others a job read would describe alike (`JobListing::observed`); and how many pages that read.
    async fn run_jobs(
        &self,
        repository: &str,
        run: u64,
        following: &BTreeSet<u64>,
    ) -> Result<RunJobs, Error> {
        let path = jobs_path(repository, run);
        let pages: Vec<Jobs> = self.get_pages(&path).await?;
        let read = pages.len();
        let (mut queued, mut seen, mut rejected) = (Vec::new(), Vec::new(), Vec::new());
        for item in pages.into_iter().flat_map(|page| page.jobs) {
            let job = Job::deserialize(&item).map_err(|source| Error::Decode {
                path: path.clone(),
                message: scrub(&source.to_string()),
            })?;
            if job.status == "queued" {
                queued.push(job);
            } else if following.contains(&job.id) {
                match ListedDetails::deserialize(&item).map(JobDetails::from) {
                    Ok(details) if is_whole(&details) => seen.push(details),
                    // Shown in part, it says nothing, and taints whatever another page shows of it.
                    Ok(_) | Err(_) => rejected.push(job.id),
                }
            }
        }
        // GitHub's name for a listing that cannot read the workflow's: the job is listed all the same, and its
        // details name it once they are read. A run with no queued job is named only as far as no read is needed.
        let named = match queued.first() {
            Some(first) => match self.workflow_name(repository, first.run_id).await {
                Ok(Some(name)) => Named::Workflow(name),
                Ok(None) => Named::GitHubs,
                Err(Unresolved) => Named::Unread(first.workflow_name.clone()),
            },
            None if seen.is_empty() => {
                return Ok(RunJobs {
                    pages: read,
                    queued: Vec::new(),
                    observed: Vec::new(),
                    rejected,
                });
            }
            None => match lock(&self.workflows).next(repository, run, Instant::now()) {
                Next::Named(name) => Named::Workflow(name),
                Next::Unnamed => Named::GitHubs,
                Next::ReadRun | Next::ReadList(_) | Next::Waiting => Named::Unread(None),
            },
        };
        // A job read names its workflow, or keeps GitHub's name when GitHub gives none; one that could not read
        // it owes the run its name. Only the first two are what a job read would give.
        match &named {
            Named::Workflow(name) => {
                for details in &mut seen {
                    details.workflow_name = Some(name.clone());
                }
            }
            Named::GitHubs => {}
            Named::Unread(_) => seen.clear(),
        }
        let workflow = match named {
            Named::Workflow(name) => Some(name),
            Named::GitHubs => queued.first().and_then(|first| first.workflow_name.clone()),
            Named::Unread(name) => name,
        };
        let queued = queued.into_iter().map(|job| QueuedJob {
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
        });
        Ok(RunJobs {
            pages: read,
            queued: queued.collect(),
            observed: seen,
            rejected,
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        self.get_page(path).await.map(|page| page.body)
    }

    /// One answer to a read nothing asks again, sent unconditionally and never kept: it makes no room in the
    /// answers kept for conditional requests, so it evicts none of them.
    async fn get_once<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        self.get_page_as(Class::of(&Method::GET, path), path, Retain::Once)
            .await
            .map(|page| page.body)
    }

    /// One answer, conditional on the last one kept for `path`, and whether GitHub's `Link` names a next page.
    async fn get_page<T: DeserializeOwned>(&self, path: &str) -> Result<Page<T>, Error> {
        self.get_page_as(Class::of(&Method::GET, path), path, Retain::Kept)
            .await
    }

    /// As `get_page`, counted as `class`, and kept or not as `retain` says.
    ///
    /// Kept: a 304 serves the answer kept for `path`, which GitHub just said is current; any other answer replaces
    /// it, kept again only when it succeeded and carries an `ETag`. So an error, a refusal or an answer without an
    /// `ETag` is never followed by the answer kept before it, and a request that fails before GitHub answers serves
    /// nothing. Once: the request is unconditional and the cache is neither read nor written.
    async fn get_page_as<T: DeserializeOwned>(
        &self,
        class: Class,
        path: &str,
        retain: Retain,
    ) -> Result<Page<T>, Error> {
        // Taken before the request, so an answer evicted meanwhile still serves a 304.
        let cached = match retain {
            Retain::Kept => lock(&self.etags).get(path, Instant::now()),
            Retain::Once => None,
        };
        let etag = cached.as_ref().map(|cached| &cached.etag);
        let response = self.send(class, Method::GET, path, None, etag).await?;
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
            // What GitHub answers now replaces what was kept, whatever it is.
            let response = match expect_success(Method::GET, path, response) {
                Ok(response) => response,
                Err(error) => {
                    if retain == Retain::Kept {
                        lock(&self.etags).remove(path);
                    }
                    return Err(error);
                }
            };
            let body: Arc<[u8]> = response.bytes.into();
            if retain == Retain::Once {
                return Ok(Page {
                    body: decode(path, &body)?,
                    next: response.next,
                });
            }
            let mut etags = lock(&self.etags);
            match etag {
                Some(etag) => etags.insert(
                    path,
                    Cached {
                        etag,
                        body: body.clone(),
                        next: response.next,
                    },
                    Instant::now(),
                ),
                None => etags.remove(path),
            }
            drop(etags);
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

    /// Every page of the listing at `path`, as `get_pages` reads them but kept or not as `retain` says, or an error
    /// when a next page is still named at `MAX_PAGES`: for a caller that must see the whole listing or nothing.
    async fn all_pages<T: DeserializeOwned>(
        &self,
        path: &str,
        retain: Retain,
    ) -> Result<Vec<T>, Error> {
        let mut pages = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = if page == 1 {
                path.to_owned()
            } else {
                format!("{path}&page={page}")
            };
            let answer: Page<T> = self
                .get_page_as(Class::of(&Method::GET, &path), &path, retain)
                .await?;
            pages.push(answer.body);
            if !answer.next {
                return Ok(pages);
            }
        }
        Err(Error::Download(format!(
            "{path} lists more than {MAX_PAGES} pages"
        )))
    }

    /// Sends a request with the token, once more with a fresh token after a 401, conditional on `etag` if given, and
    /// reads its answer. Records the rate limit.
    async fn send(
        &self,
        class: Class,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
        etag: Option<&HeaderValue>,
    ) -> Result<Response, Error> {
        let Ok(answered) = self
            .exchange(class, &method, path, body, etag, || async {
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
        class: Class,
        method: &Method,
        path: &str,
        body: Option<&serde_json::Value>,
        etag: Option<&HeaderValue>,
        permit: impl Fn() -> Allowed,
    ) -> Result<Result<Answered, W>, Error> {
        if let Some(until) = self.blocked_until() {
            return Err(Error::RateLimited { until });
        }
        // The token GitHub refused, once it did.
        let mut refused: Option<Token> = None;
        // Whether a token expired while the permit was asked, once it did: a fresh one is read once more.
        let mut expired = false;
        loop {
            let granted = self.tokens.get(refused.as_ref()).await?;
            if let Err(withdrawn) = permit().await {
                return Ok(Err(withdrawn));
            }
            // The permit is awaited with the token in hand: one that expired meanwhile is not sent, and the permit is
            // asked again with the next.
            if !self.tokens.usable(&granted) {
                if expired {
                    return Err(Error::Token(
                        "the token expired while the request waited for it".to_owned(),
                    ));
                }
                expired = true;
                continue;
            }
            let token = granted.token().clone();
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
            let _ = REQUESTS.try_with(|sent| sent.set(sent.get() + 1));
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
    async fn queued_jobs(
        &self,
        repository: &str,
        following: &BTreeSet<u64>,
    ) -> Result<JobListing, Error> {
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
        let mut observed = Vec::new();
        let mut rejected = HashSet::new();
        let mut requests = 0;
        for &(_, run) in &runs {
            if requests >= JOB_REQUESTS {
                break;
            }
            let read = self.run_jobs(repository, run, following).await?;
            requests += read.pages;
            jobs.extend(read.queued);
            observed.extend(read.observed);
            rejected.extend(read.rejected);
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
        // A job GitHub moved between pages shows twice. Queued on either page, it is the listing's; shown otherwise
        // on each, or in part on one, nothing says which is newer, so it is read.
        let observed = agreed(observed)
            .into_iter()
            .filter(|details| !rejected.contains(&details.id))
            .filter(|details| {
                jobs.binary_search_by_key(&details.id, |job| job.id)
                    .is_err()
            })
            .collect();
        Ok(JobListing {
            queued: jobs,
            observed,
        })
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
        let response = self
            .send(Class::Registration, Method::POST, &path, Some(&body), None)
            .await?;
        let response = expect_success(Method::POST, &path, response)?;
        let jit: JitResponse = decode(&path, &response.bytes)?;
        Ok((jit.runner, JitConfig(jit.encoded_jit_config)))
    }

    async fn delete_runner(&self, repository: &str, id: u64) -> Result<Deleted, Error> {
        let path = format!("/repos/{repository}/actions/runners/{id}");
        let response = self
            .send(Class::Delete, Method::DELETE, &path, None, None)
            .await?;
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
        // Kept for a conditional request (ADR 0179): every job of a run asks for it again, and an unchanged run is a
        // 304, which costs no rate limit. Each decision still asks GitHub: a re-run keeps the run's ID and changes
        // who triggered it.
        let run: RunBody = self
            .get_page_as(Class::Trigger, &path, Retain::Kept)
            .await?
            .body;
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
        // Kept for a conditional request, asked again for each decision: its head and base can change (ADR 0179).
        let pull: PullRequestBody = self.get(&path).await?;
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
        // Kept for a conditional request, asked again for each decision: a branch moves (ADR 0179).
        let head = self.get::<BranchBody>(&path).await?.commit.sha;
        if !commit(&head) {
            return Ok(false);
        }
        // The commits it lists are not needed: one is the fewest it sends.
        let path = format!("/repos/{repository}/compare/{head}...{sha}?per_page=1");
        // Kept for a conditional request: the jobs of one run compare the same two commits.
        let body: Result<CompareBody, Error> = self.get(&path).await;
        // `behind`: the branch has moved on past it.
        Ok(matches!(body?.status.as_str(), "identical" | "behind"))
    }

    async fn cancel_run(&self, repository: &str, run: u64) -> Result<(), Error> {
        let path = format!("/repos/{repository}/actions/runs/{run}/cancel");
        let response = self
            .send(Class::Cancel, Method::POST, &path, None, None)
            .await?;
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
            .exchange(
                Class::of(&Method::POST, &path),
                &Method::POST,
                &path,
                None,
                None,
                allowed,
            )
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
            failed: body.conclusion.as_deref() == Some("failure"),
        })
    }

    async fn attempt_jobs(
        &self,
        repository: &str,
        run: u64,
        attempt: u32,
    ) -> Result<Vec<AttemptJob>, ActionFailure> {
        let path = format!(
            "/repos/{repository}/actions/runs/{run}/attempts/{attempt}/jobs?per_page={PER_PAGE}"
        );
        // Read once per re-run: nothing asks for these pages again, so none is kept.
        let read = self.all_pages::<AttemptJobs>(&path, Retain::Once).await;
        let pages = read.map_err(|error| ActionFailure::of(&error))?;
        // Every page says how many jobs the attempt has, and they agree: a page without the count, or a count that
        // moved between pages, is no whole listing.
        let mut totals = pages.iter().map(|page| page.total_count);
        let total = match totals.next().flatten() {
            Some(total) if totals.all(|other| other == Some(total)) => total,
            _ => return Err(ActionFailure::Broken),
        };
        let mut jobs: Vec<AttemptJob> = pages.into_iter().flat_map(|page| page.jobs).collect();
        jobs.sort_by_key(|job| job.id);
        jobs.dedup_by_key(|job| job.id);
        // A job left unread could be one that vetoes the re-run: all of them, or a failed read.
        if total == jobs.len() {
            Ok(jobs)
        } else {
            Err(ActionFailure::Broken)
        }
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

/// What a listing read of one run's jobs.
struct RunJobs {
    /// The pages read.
    pages: usize,
    queued: Vec<QueuedJob>,
    observed: Vec<JobDetails>,
    /// The followed jobs not queued that some page showed without the whole answer.
    rejected: Vec<u64>,
}

/// How a run's workflow is named for its jobs a listing read.
enum Named {
    /// By its workflow's name.
    Workflow(String),
    /// By GitHub's name for the run: GitHub gives the run no workflow, or does not list it.
    GitHubs,
    /// Not yet: its name takes a read that failed or that the listing did not make. A queued job keeps this name,
    /// GitHub's; no other job's details are given.
    Unread(Option<String>),
}

/// Whether a listed job's details say all a job read would for its status, its times readable: a waiting job has none
/// of a start, an end or a runner; a job in progress has its runner and its start and no end; a completed one its
/// conclusion and its end, a runner and a start or neither, and every step completed, so no step is left to come. A queued
/// job is the listing's, and a status the history does not know is not given.
pub(crate) fn is_whole(details: &JobDetails) -> bool {
    let readable =
        |at: &Option<String>| at.as_deref().is_none_or(|at| time::parse_ms(at).is_some());
    let steps_readable = details
        .steps
        .iter()
        .all(|step| readable(&step.started_at) && readable(&step.completed_at));
    if !(readable(&details.created_at)
        && readable(&details.started_at)
        && readable(&details.completed_at)
        && steps_readable)
    {
        return false;
    }
    let shown = (
        details.runner_name.is_some(),
        details.started_at.is_some(),
        details.completed_at.is_some(),
    );
    // What each status has: (runner, start, end).
    match details.status.as_str() {
        "waiting" => shown == (false, false, false),
        "in_progress" => shown == (true, true, false),
        "completed" => {
            details.conclusion.is_some()
                && matches!(shown, (false, false, true) | (true, true, true))
                && details.steps.iter().all(|step| step.status == "completed")
        }
        _ => false,
    }
}

/// The observed jobs by ID, each once: a job shown twice alike is kept once, and one shown twice otherwise, which a
/// listing cannot order, is left out.
fn agreed(mut observed: Vec<JobDetails>) -> Vec<JobDetails> {
    observed.sort_by_key(|details| details.id);
    let mut agreed: Vec<JobDetails> = Vec::with_capacity(observed.len());
    let mut conflicting = HashSet::new();
    for details in observed {
        match agreed.last() {
            Some(last) if last.id == details.id => {
                if *last != details {
                    conflicting.insert(details.id);
                }
            }
            _ => agreed.push(details),
        }
    }
    agreed.retain(|details| !conflicting.contains(&details.id));
    agreed
}

/// A job as a run's jobs page shows it, read strictly: each field a job read has must be there, `null` where GitHub
/// gives none, where `JobDetails` takes a missing one for its default. An item without one is not taken for a read.
#[derive(Deserialize)]
struct ListedDetails {
    id: u64,
    run_id: u64,
    run_attempt: u32,
    name: String,
    #[serde(deserialize_with = "present")]
    workflow_name: Option<String>,
    #[serde(deserialize_with = "present")]
    head_branch: Option<String>,
    #[serde(deserialize_with = "present")]
    html_url: Option<String>,
    labels: Vec<String>,
    #[serde(deserialize_with = "present")]
    created_at: Option<String>,
    status: String,
    #[serde(deserialize_with = "present")]
    conclusion: Option<String>,
    #[serde(deserialize_with = "present")]
    started_at: Option<String>,
    #[serde(deserialize_with = "present")]
    completed_at: Option<String>,
    #[serde(deserialize_with = "present")]
    runner_name: Option<String>,
    steps: Vec<ListedStep>,
}

/// A step of `ListedDetails`, read as strictly.
#[derive(Deserialize)]
struct ListedStep {
    number: u32,
    name: String,
    status: String,
    #[serde(deserialize_with = "present")]
    conclusion: Option<String>,
    #[serde(deserialize_with = "present")]
    started_at: Option<String>,
    #[serde(deserialize_with = "present")]
    completed_at: Option<String>,
}

/// A field that may be `null` but must be there: with `deserialize_with`, serde no longer takes a missing `Option`
/// for `None`.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

impl From<ListedDetails> for JobDetails {
    fn from(listed: ListedDetails) -> JobDetails {
        JobDetails {
            id: listed.id,
            run_id: listed.run_id,
            run_attempt: listed.run_attempt,
            name: listed.name,
            workflow_name: listed.workflow_name,
            head_branch: listed.head_branch,
            html_url: listed.html_url,
            labels: listed.labels,
            created_at: listed.created_at,
            status: listed.status,
            conclusion: listed.conclusion,
            started_at: listed.started_at,
            completed_at: listed.completed_at,
            runner_name: listed.runner_name,
            steps: listed
                .steps
                .into_iter()
                .map(|step| JobStep {
                    number: step.number,
                    name: step.name,
                    status: step.status,
                    conclusion: step.conclusion,
                    started_at: step.started_at,
                    completed_at: step.completed_at,
                })
                .collect(),
        }
    }
}

struct Response {
    status: StatusCode,
    etag: Option<HeaderValue>,
    /// Whether its `Link` names a next page.
    next: bool,
    bytes: Vec<u8>,
}

/// Whether a read keeps its answer for a conditional request on its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retain {
    /// Kept: the path is asked again, and an unchanged answer is a 304.
    Kept,
    /// Never kept, nor asked conditionally: a read nothing asks again, which must not make room in the cache.
    Once,
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
/// Only a 304 serves a kept answer (ADR 0179). Nothing kept survives a restart, and a reload does not touch it.
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
        message: scrub(&message),
    })
}

fn decode<T: DeserializeOwned>(path: &str, bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|source| Error::Decode {
        path: path.to_owned(),
        message: scrub(&source.to_string()),
    })
}

/// The longest text from an answer an error keeps, in characters.
const SCRUBBED_CHARS: usize = 300;

/// What GitHub sent back, or a parser said of it, as an error may keep it: GitHub's tokens (`ghp_`, `gho_`, `ghu_`,
/// `ghs_`, `ghr_`, `github_pat_`), JWTs (`eyJ…`) and a bearer's credential each replaced by `<redacted>`, all from a
/// PEM armour line on dropped, and cut at [`SCRUBBED_CHARS`]. GitHub does not echo a credential, but an error must not
/// rely on that.
pub(crate) fn scrub(text: &str) -> String {
    const PREFIXES: [&str; 8] = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "eyJ",
        "Bearer ",
    ];
    let text = text.find("-----BEGIN").map_or(text, |at| &text[..at]);
    let in_word =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | '=');
    let mut out = String::new();
    let mut rest = text;
    let mut previous: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        let starts = previous.is_none_or(|p| !p.is_ascii_alphanumeric() && p != '_');
        if starts && let Some(prefix) = PREFIXES.iter().find(|prefix| rest.starts_with(**prefix)) {
            let after = &rest[prefix.len()..];
            let end = after.find(|c: char| !in_word(c)).unwrap_or(after.len());
            if prefix.starts_with("Bearer") {
                out.push_str("Bearer ");
            }
            out.push_str("<redacted>");
            rest = &after[end..];
            previous = Some('>');
            continue;
        }
        out.push(c);
        previous = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    if out.chars().count() > SCRUBBED_CHARS {
        out = out.chars().take(SCRUBBED_CHARS).collect();
        out.push('…');
    }
    out
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
