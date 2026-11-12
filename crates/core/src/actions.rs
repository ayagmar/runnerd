//! The owner's job actions (ADR 0134): cancelling a job's workflow run and re-running a job, or its run's failed jobs,
//! on GitHub, for `POST /v1/jobs/{id}/cancel` and `POST /v1/jobs/{id}/rerun`. A request without a confirmation gets a
//! preview: the job and its run's other jobs from the history, the run's current attempt read from GitHub, and a
//! confirmation issued for exactly that (`flights`). A confirmed request is authorized against one snapshot of the
//! config (`policy`) and sent once through the engine's own GitHub client, off the engine's loop. Once the token is
//! in hand, and before each send (a 401's second included), the snapshot is checked, the run read again for its
//! attempt, and the snapshot checked once more, with nothing in between that and the send.
//!
//! It must not write the job history (GitHub's listings bring what the action did), kill a runner, keep a deadline,
//! send an ambiguous request again, or put GitHub's text or the token anywhere: a refusal is its class and status.

mod flights;
pub mod policy;

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tracing::info;

use self::flights::{Boarding, Flights, Issued, Landed, Redeemed};
use self::policy::{Denied, Policy};
use crate::api::{
    self, ActionAnswer, ActionPreview, ActionRefusal, ActionResult, ActionTarget, JobAction,
    JobSummary, PreviewFacts, RunSeen, SIBLINGS_MAX, Superseded, UnknownCause,
};
use crate::github::{ActionFailure, GithubApi, JobActionRequest, Permit, RunNow};
use crate::scheduler::engine::{Control, Shutdown};
use crate::store::{Store, StoreError};
use crate::time;

/// How long one action may take to send, its token read, its read of the run and a 401's second try included: below
/// the 30 s a client waits for an answer, so the client hears the outcome, if only that it is unknown. A preview's
/// read of the run has as long.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// How long the job actions wait for GitHub, and how long a confirmation lasts.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// For one action, or one preview's read of the run.
    pub deadline: Duration,
    /// For a confirmation to be sent back.
    pub confirmation: Duration,
}

/// The daemon's limits: [`DEADLINE`] and [`api::CONFIRM_SECS`].
pub const LIMITS: Limits = Limits {
    deadline: DEADLINE,
    confirmation: Duration::from_secs(api::CONFIRM_SECS),
};

/// The GitHub calls the job actions make. Every [`GithubApi`] is one, so the engine's own client sends them; a test
/// gives a fake.
pub trait ActionsGithub: Send + Sync + 'static {
    /// Reads the run's current attempt and whether it completed.
    fn run<'call>(
        &'call self,
        repository: &'call str,
        run: u64,
    ) -> BoxFuture<'call, Result<RunNow, ActionFailure>>;

    /// Sends `request` once, asking `permit` once the token is in hand and before each send; GitHub's success
    /// status, or why not.
    fn send<'call>(
        &'call self,
        repository: &'call str,
        request: JobActionRequest,
        permit: &'call Permit<'call>,
    ) -> BoxFuture<'call, Result<u16, ActionFailure>>;
}

impl<G: GithubApi + 'static> ActionsGithub for G {
    fn run<'call>(
        &'call self,
        repository: &'call str,
        run: u64,
    ) -> BoxFuture<'call, Result<RunNow, ActionFailure>> {
        Box::pin(GithubApi::action_run(self, repository, run))
    }

    fn send<'call>(
        &'call self,
        repository: &'call str,
        request: JobActionRequest,
        permit: &'call Permit<'call>,
    ) -> BoxFuture<'call, Result<u16, ActionFailure>> {
        Box::pin(GithubApi::job_action(self, repository, request, permit))
    }
}

/// What a request got.
#[derive(Debug)]
pub enum Asked {
    /// It was not confirmed, or its confirmation no longer holds: nothing was sent (409).
    Preview(Box<ActionPreview>),
    /// GitHub was asked, by this request or by the same one in flight.
    Done(ActionResult),
}

/// Why a request was answered before GitHub was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionError {
    /// No such job in the history (404).
    #[error("{0}")]
    NotFound(String),
    /// Job actions are off, or the repository is no longer configured (403).
    #[error("{0}")]
    Forbidden(String),
    /// The request cannot apply to this job (422).
    #[error("{0}")]
    Invalid(String),
    /// Another action is in flight for the run (409).
    #[error("{0}")]
    Busy(String),
    /// The history could not be read, or no confirmation could be made (500).
    #[error("{0}")]
    Failed(String),
    /// runnerd is stopping (503).
    #[error("runnerd is stopping")]
    Stopped,
}

/// Sends the owner's job actions. Cloned into each request; every clone shares the confirmations and flights.
#[derive(Clone)]
pub struct JobActions {
    github: Arc<dyn ActionsGithub>,
    store: Store,
    control: Arc<Control>,
    flights: Arc<Flights>,
    deadline: Duration,
}

impl std::fmt::Debug for JobActions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobActions").finish_non_exhaustive()
    }
}

impl JobActions {
    /// Actions sent through `github`, for the jobs `store` has, under the config `control` holds, within `limits`
    /// ([`LIMITS`] but in tests).
    pub fn new(
        github: Arc<dyn ActionsGithub>,
        store: Store,
        control: Arc<Control>,
        limits: Limits,
    ) -> JobActions {
        JobActions {
            github,
            store,
            control,
            flights: Arc::new(Flights::new(limits.confirmation)),
            deadline: limits.deadline,
        }
    }

    /// Asks `action` of job `job`. Unless `confirm` is a confirmation this daemon issued with a preview of this very
    /// action and job, still current, the answer is a new preview and nothing is sent. A confirmed one is sent once,
    /// or joins the same one in flight for the run; another action in flight for the run refuses it.
    pub async fn ask(
        &self,
        job: u64,
        action: JobAction,
        confirm: Option<&str>,
    ) -> Result<Asked, ActionError> {
        if action == JobAction::Unsupported {
            return Err(ActionError::Invalid("no such job action".to_owned()));
        }
        if self.control.mode() != Shutdown::No {
            return Err(ActionError::Stopped);
        }
        let summary = self
            .store
            .job(job)
            .await
            .map_err(|error| read_failed(&error))?
            .ok_or_else(|| ActionError::NotFound(format!("no job {job} in the history")))?
            .summary;
        if summary.run_id == 0 {
            return Err(ActionError::Invalid(api::actions_no_run(job)));
        }
        // The config of this moment, one snapshot: a reload since the preview may have turned actions off or removed
        // the repository.
        let policy = self.control.action_policy();
        allowed(&policy, &summary.repository)?;
        let superseded = match confirm {
            None => None,
            Some(token) => {
                match self
                    .flights
                    .redeem(token, action, job, policy.revision(), Instant::now())
                {
                    Redeemed::Board { boarding, issued } => {
                        return self.board(boarding, issued, &summary).await;
                    }
                    Redeemed::Stale(why) => Some(why),
                }
            }
        };
        let run = self.read_run(&summary.repository, summary.run_id).await;
        self.preview(&summary, action, run, superseded, &policy)
            .await
    }

    /// The preview of `action` on `summary`'s job with the run as `run` says, and a confirmation for it if the run was
    /// read.
    async fn preview(
        &self,
        summary: &JobSummary,
        action: JobAction,
        run: RunSeen,
        superseded: Option<Superseded>,
        policy: &Policy,
    ) -> Result<Asked, ActionError> {
        let others = self
            .store
            .run_jobs(
                &summary.repository,
                summary.run_id,
                summary.id,
                SIBLINGS_MAX,
            )
            .await
            .map_err(|error| read_failed(&error))?;
        let mut target = ActionTarget {
            action,
            job: summary.id,
            name: summary.name.clone(),
            repository: summary.repository.clone(),
            run: summary.run_id,
            attempt: None,
        };
        let confirm = match run {
            RunSeen::Now { attempt, .. } => {
                target.attempt = Some(attempt);
                let issued = Issued {
                    target: target.clone(),
                    revision: policy.revision(),
                };
                Some(
                    self.flights
                        .issue(issued, Instant::now())
                        .map_err(|error| {
                            ActionError::Failed(format!("cannot make a confirmation: {error}"))
                        })?,
                )
            }
            RunSeen::Unread { .. } => None,
        };
        Ok(Asked::Preview(Box::new(ActionPreview::new(PreviewFacts {
            paused: self.control.is_paused(&target.repository),
            target,
            job_attempt: summary.run_attempt,
            run,
            superseded,
            confirm,
            siblings: others.jobs,
            more: others.more,
        }))))
    }

    /// Sends what `issued` names, or joins it in flight, and answers as it lands.
    async fn board(
        &self,
        boarding: Boarding,
        issued: Issued,
        summary: &JobSummary,
    ) -> Result<Asked, ActionError> {
        let answer = match boarding {
            Boarding::Busy(other) => {
                return Err(ActionError::Busy(api::actions_in_flight(
                    other,
                    issued.target.run,
                )));
            }
            Boarding::Follow(answer) => answer,
            Boarding::Lead { landing, answer } => {
                // A task of its own: a client that goes away neither stops the request midway nor leaves the flight.
                let actions = self.clone();
                let sent = issued.clone();
                tokio::spawn(async move {
                    let landed = actions.dispatch(&sent).await;
                    landing.land(landed);
                });
                answer
            }
        };
        match wait(answer, &issued.target).await {
            Landed::Done(result) => Ok(Asked::Done(result)),
            Landed::Repreview { run, why } => {
                let policy = self.control.action_policy();
                allowed(&policy, &summary.repository)?;
                self.preview(summary, issued.target.action, run, Some(why), &policy)
                    .await
            }
        }
    }

    /// Reads the run for a preview, within the deadline, unless a stop comes first.
    async fn read_run(&self, repository: &str, run: u64) -> RunSeen {
        tokio::select! {
            biased;
            () = self.control.stop_asked() => RunSeen::Unread { status: None },
            read = tokio::time::timeout(self.deadline, self.github.run(repository, run)) => match read {
                Ok(Ok(now)) => seen(now),
                Ok(Err(failure)) => RunSeen::Unread { status: status_of(failure) },
                Err(_) => RunSeen::Unread { status: None },
            },
        }
    }

    /// Sends `issued`'s action once, within the deadline, unless a stop comes first.
    async fn dispatch(&self, issued: &Issued) -> Landed {
        let target = &issued.target;
        let unknown = |cause| {
            Landed::Done(ActionResult::new(
                target,
                ActionAnswer::Unknown {
                    cause,
                    status: None,
                },
                time::now_ms() / 1000,
            ))
        };
        tokio::select! {
            biased;
            () = self.control.stop_asked() => unknown(UnknownCause::Stopping),
            landed = tokio::time::timeout(self.deadline, self.send(issued)) => {
                landed.unwrap_or_else(|_| unknown(UnknownCause::TimedOut))
            },
        }
    }

    /// Sends the action once, if, once the token is in hand, the config is the confirmation's and the run is at the
    /// attempt it showed.
    async fn send(&self, issued: &Issued) -> Landed {
        let target = &issued.target;
        let repository = &target.repository;
        let request = match target.action {
            JobAction::Cancel => JobActionRequest::CancelRun { run: target.run },
            JobAction::Rerun => JobActionRequest::RerunJob { job: target.job },
            JobAction::RerunFailed | JobAction::Unsupported => {
                JobActionRequest::RerunFailed { run: target.run }
            }
        };
        // Why the permit said no, when it did.
        let withheld: std::sync::Mutex<Option<Landed>> = std::sync::Mutex::new(None);
        let permit = || -> BoxFuture<'_, bool> {
            Box::pin(async {
                let why = self.verify(issued).await;
                let go = why.is_none();
                *crate::sync::lock(&withheld) = why;
                go
            })
        };
        let (slug, run, job) = (target.action.slug(), target.run, target.job);
        info!("{repository}: sending the {slug} of run {run} (job {job}) to GitHub");
        let answer = match self.github.send(repository, request, &permit).await {
            Err(ActionFailure::Withdrawn) => match crate::sync::lock(&withheld).take() {
                Some(landed) => return landed,
                None => classify(Err(ActionFailure::Withdrawn)),
            },
            // A 409 says the run cannot be cancelled now, not that it completed: only a read of it says that.
            Err(ActionFailure::Status { status: 409 }) if target.action == JobAction::Cancel => {
                match self.github.run(repository, target.run).await {
                    Ok(now) if now.completed => ActionAnswer::AlreadyFinished { status: 409 },
                    _ => ActionAnswer::Refused {
                        refusal: ActionRefusal::Conflict,
                        status: Some(409),
                        until: None,
                    },
                }
            }
            sent => classify(sent),
        };
        let result = ActionResult::new(target, answer, time::now_ms() / 1000);
        // The class and status only: GitHub's text never reaches a log.
        info!(
            "{repository}: GitHub's answer to the {slug} of run {run}: {:?}, status {:?}, {:?}",
            result.outcome, result.status, result.refusal
        );
        Landed::Done(result)
    }

    /// Why `issued` may not go now, if it may not: the run is not at the attempt it showed, or the config changed
    /// since its confirmation. The config is read after the run, so nothing but the send follows the check.
    async fn verify(&self, issued: &Issued) -> Option<Landed> {
        let target = &issued.target;
        let withdrawn = || {
            Landed::Done(ActionResult::new(
                target,
                classify(Err(ActionFailure::Withdrawn)),
                time::now_ms() / 1000,
            ))
        };
        match self.github.run(&target.repository, target.run).await {
            Ok(now) if Some(now.attempt) == target.attempt => {}
            Ok(now) => {
                return Some(Landed::Repreview {
                    run: seen(now),
                    why: Superseded::AttemptChanged,
                });
            }
            Err(failure) => {
                return Some(Landed::Repreview {
                    run: RunSeen::Unread {
                        status: status_of(failure),
                    },
                    why: Superseded::Unverified,
                });
            }
        }
        (self.control.action_policy().revision() != issued.revision).then(withdrawn)
    }

    /// How many runs have an action in flight.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.flights.len()
    }

    /// How many requests wait for an answer from GitHub, the lead of each flight included.
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.flights.waiting()
    }
}

/// Whether `policy` lets the owner act on `repository`'s jobs.
fn allowed(policy: &Policy, repository: &str) -> Result<(), ActionError> {
    policy.allows(repository).map_err(|denied| {
        ActionError::Forbidden(match denied {
            Denied::Off => api::actions_off().to_owned(),
            Denied::NotConfigured => api::actions_not_configured(repository),
        })
    })
}

/// What the flight lands; if it ends without an answer, as when the runtime stops, the outcome is unknown.
async fn wait(mut answer: watch::Receiver<Option<Landed>>, target: &ActionTarget) -> Landed {
    let stopped = || {
        Landed::Done(ActionResult::new(
            target,
            ActionAnswer::Unknown {
                cause: UnknownCause::Stopping,
                status: None,
            },
            time::now_ms() / 1000,
        ))
    };
    match answer.wait_for(Option::is_some).await {
        Ok(landed) => landed.clone().unwrap_or_else(stopped),
        Err(_) => stopped(),
    }
}

fn seen(now: RunNow) -> RunSeen {
    RunSeen::Now {
        attempt: now.attempt,
        completed: now.completed,
    }
}

/// The HTTP status GitHub answered with, if it did.
fn status_of(failure: ActionFailure) -> Option<u16> {
    match failure {
        ActionFailure::Status { status } => Some(status),
        ActionFailure::RateLimited { status, .. } => status,
        _ => None,
    }
}

fn read_failed(error: &StoreError) -> ActionError {
    ActionError::Failed(format!("cannot read the job history: {error}"))
}

/// What GitHub's answer says, as a class and a status: never its text.
fn classify(sent: Result<u16, ActionFailure>) -> ActionAnswer {
    let refused = |refusal, status| ActionAnswer::Refused {
        refusal,
        status,
        until: None,
    };
    match sent {
        Ok(status) => ActionAnswer::Accepted { status },
        Err(ActionFailure::NoToken) => refused(ActionRefusal::NoToken, None),
        Err(ActionFailure::Withdrawn) => refused(ActionRefusal::Withdrawn, None),
        // Nothing left this machine.
        Err(ActionFailure::Unreachable) => refused(ActionRefusal::Unreachable, None),
        Err(ActionFailure::RateLimited { status, until }) => ActionAnswer::Refused {
            refusal: ActionRefusal::RateLimited,
            status,
            until: Some(
                until
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
        },
        Err(ActionFailure::Status { status }) => {
            let code = Some(status);
            match status {
                401 => refused(ActionRefusal::Unauthorized, code),
                403 => refused(ActionRefusal::Forbidden, code),
                404 | 410 => refused(ActionRefusal::NotFound, code),
                409 => refused(ActionRefusal::Conflict, code),
                422 => refused(ActionRefusal::Unprocessable, code),
                // A server error does not say whether GitHub acted before it failed.
                500..=599 => ActionAnswer::Unknown {
                    cause: UnknownCause::ServerError,
                    status: code,
                },
                _ => refused(ActionRefusal::Failed, code),
            }
        }
        Err(ActionFailure::TimedOut) => ActionAnswer::Unknown {
            cause: UnknownCause::TimedOut,
            status: None,
        },
        Err(ActionFailure::Broken) => ActionAnswer::Unknown {
            cause: UnknownCause::Broken,
            status: None,
        },
    }
}

#[cfg(test)]
mod tests;
