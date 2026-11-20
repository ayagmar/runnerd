//! Re-runs a run whose runner failed one of its jobs, once per attempt, through the job actions' own path (ADR 0153):
//! GitHub's re-run of the run's failed jobs, without a preview, on the engine's own GitHub client.
//!
//! The rule is stateless: a run is re-run only while GitHub says its current attempt is the one the fault happened in,
//! it completed, and every job of that attempt that failed or was cancelled is one the runner failed, and the permit
//! reads the attempt once more right before the send. Once GitHub accepted, the run is at a later attempt, so neither
//! this daemon nor a restarted one sends it again; a person's re-run moves it the same way. Faults of a repository
//! whose `rerun_runner_faults` or `[github] job_actions` is off, or that the config no longer has, wait unread until a
//! reload turns them on or the run is no longer followed. A refusal
//! that asking again soon cannot change, such as a token without Actions: write, holds the repository's re-runs for an
//! hour or until the config changes, with one attention item; its runs wait for the hold's end.
//!
//! It must not re-run a run from its third attempt on, send a held repository's re-run, read GitHub for a run more than once a
//! minute, or write the history beyond noting the re-run GitHub accepted.

use std::time::Duration;

use tokio::time::Instant;

use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use super::faults::{Due, RunKey};
use super::flights::{Boarding, Issued, Landed};
use super::{JobActions, classify, wait};
use crate::api::{
    ActionAnswer, ActionOutcome, ActionRefusal, ActionResult, ActionTarget, Event, JobAction,
    OutcomeKind, RerunRefused, RunSeen, Superseded,
};
use crate::github::{ActionFailure, AttemptJob};
use crate::time;

/// How far back a start reads the history's faults: as long as a run is followed.
const LOOKBACK: Duration = super::faults::FOLLOWED;

/// How long until a run that has not completed, or a run busy with another action, is read again.
const POLL: Duration = Duration::from_secs(60);

/// How long until a check that failed is made again.
const BACKOFF: Duration = Duration::from_secs(120);

/// The attempts a fault is re-run from: the first and the second, so a run reaches at most its third by runnerd.
const ATTEMPTS_MAX: u32 = 2;

/// What one check of a run came to.
enum Checked {
    /// Nothing more to do for the attempt.
    Settled,
    /// Check again at this time; no failure.
    Wait(Instant),
    /// The check failed; check again later, a few times.
    Failed,
}

impl JobActions {
    /// Follows the history's runner faults and re-runs their runs, until a stop. Spawned once by the engine.
    pub async fn rerun_faults(self) {
        let mut events = self.store.subscribe();
        self.read_faults(Instant::now()).await;
        loop {
            let next = self.faults.next();
            let due = async {
                match next {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = self.control.stop_asked() => return,
                event = events.recv() => match event {
                    Ok(envelope) => {
                        if let Event::JobFinished { job } = &envelope.event
                            && job.outcome().kind == OutcomeKind::RunnerFault
                        {
                            self.faults.seen(
                                RunKey { repository: job.repository.clone(), run: job.run_id },
                                job.run_attempt,
                                job.id,
                                Instant::now(),
                            );
                        }
                    }
                    // Events were missed: the history has the faults among them.
                    Err(RecvError::Lagged(_)) => self.read_faults(Instant::now()).await,
                    Err(RecvError::Closed) => return,
                },
                () = due => self.pass(Instant::now()).await,
            }
        }
    }

    /// Checks every run whose check is due at `now`, one after the other.
    pub(super) async fn pass(&self, now: Instant) {
        for due in self.faults.due(now) {
            match self.check(&due, now).await {
                Checked::Settled => self.faults.settle(&due),
                Checked::Wait(at) => self.faults.wait(&due, at),
                Checked::Failed => {
                    if !self.faults.failed(&due, now + BACKOFF) {
                        warn!(
                            "{}: gave up re-running run {} after {} failed checks",
                            due.key.repository,
                            due.key.run,
                            super::faults::TRIES_MAX
                        );
                    }
                }
            }
        }
    }

    /// Follows the faults the history has from the last [`LOOKBACK`] that runnerd has not re-run.
    pub(super) async fn read_faults(&self, now: Instant) {
        let since =
            time::now_ms().saturating_sub(LOOKBACK.as_millis().try_into().unwrap_or(u64::MAX));
        match self.store.runner_faults(since).await {
            Ok(faults) => {
                for fault in faults {
                    self.faults.seen(
                        RunKey {
                            repository: fault.repository,
                            run: fault.run,
                        },
                        fault.attempt,
                        fault.job,
                        now,
                    );
                }
            }
            Err(error) => warn!("cannot read the runner faults to re-run: {error}"),
        }
    }

    /// Checks `due`'s run at `now` and re-runs its failed jobs if the rule says so.
    async fn check(&self, due: &Due, now: Instant) -> Checked {
        let RunKey { repository, run } = &due.key;
        let (repository, run) = (repository.as_str(), *run);
        if due.attempt > ATTEMPTS_MAX {
            info!(
                "{repository}: not re-running run {run}: its runner failed attempt {}, and runnerd re-runs only the \
                 first {ATTEMPTS_MAX}",
                due.attempt
            );
            return Checked::Settled;
        }
        let policy = self.control.action_policy();
        // Off, or not configured: a reload may turn it on again while the run is followed. Nothing is read meanwhile.
        if policy.reruns(repository).is_err() {
            return Checked::Wait(now + POLL);
        }
        if let Some(held) = self.faults.held(repository, policy.revision(), now) {
            return Checked::Wait(held.until);
        }
        match self.read(self.github.run(repository, run)).await {
            Ok(read) if read.attempt != due.attempt => {
                info!(
                    "{repository}: not re-running run {run}: it is at attempt {} already",
                    read.attempt
                );
                return Checked::Settled;
            }
            Ok(read) if !read.completed => return Checked::Wait(now + POLL),
            Ok(_) => {}
            Err(failure) => return waited(failure, now),
        }
        let jobs = match self
            .read(self.github.attempt_jobs(repository, run, due.attempt))
            .await
        {
            Ok(jobs) => jobs,
            Err(failure) => return waited(failure, now),
        };
        let faulted = match self.faulted(&jobs).await {
            Ok(faulted) => faulted,
            Err(Unfit::Unfinished) => return Checked::Wait(now + POLL),
            Err(Unfit::Other(job)) => {
                info!(
                    "{repository}: not re-running run {run}: job {job} of attempt {} failed or was cancelled, and \
                     not by its runner",
                    due.attempt
                );
                return Checked::Settled;
            }
            Err(Unfit::Unread) => return Checked::Failed,
        };
        if faulted.is_empty() {
            return Checked::Settled;
        }
        let issued = Issued {
            target: ActionTarget {
                action: JobAction::RerunFailed,
                job: due.job,
                name: String::new(),
                repository: repository.to_owned(),
                run,
                attempt: Some(due.attempt),
            },
            revision: policy.revision(),
        };
        let answer = match self.flights.board(&issued) {
            Boarding::Busy(_) => return Checked::Wait(now + POLL),
            Boarding::Follow(answer) => answer,
            Boarding::Lead { landing, answer } => {
                let landed = self.dispatch(&issued).await;
                landing.land(landed);
                answer
            }
        };
        let result = match wait(answer, &issued.target).await {
            Landed::Done(result) => result,
            Landed::Repreview {
                why: Superseded::AttemptChanged,
                run: seen,
            } => {
                let attempt = match seen {
                    RunSeen::Now { attempt, .. } => attempt.to_string(),
                    RunSeen::Unread { .. } => "a later one".to_owned(),
                };
                info!(
                    "{repository}: not re-running run {run}: it moved to attempt {attempt} before the send"
                );
                return Checked::Settled;
            }
            Landed::Repreview { .. } => return Checked::Failed,
        };
        self.landed(due, &faulted, &result, policy.revision(), now)
    }

    /// What `result`, GitHub's answer to the re-run of `due`'s run, comes to.
    fn landed(
        &self,
        due: &Due,
        faulted: &[u64],
        result: &ActionResult,
        revision: u64,
        now: Instant,
    ) -> Checked {
        let RunKey { repository, run } = &due.key;
        match (result.outcome, result.refusal) {
            (ActionOutcome::Accepted | ActionOutcome::AlreadyFinished, _) => {
                info!(
                    "{repository}: re-ran the failed jobs of run {run} automatically, as attempt {}: its runner \
                     failed job {}",
                    due.attempt + 1,
                    due.job
                );
                self.store.reran(faulted.to_vec(), time::now_ms());
                Checked::Settled
            }
            (ActionOutcome::Refused, Some(ActionRefusal::RateLimited)) => {
                Checked::Wait(until_instant(result.until, now))
            }
            (
                ActionOutcome::Refused,
                Some(
                    ActionRefusal::NoToken
                    | ActionRefusal::Unauthorized
                    | ActionRefusal::Forbidden
                    | ActionRefusal::NotFound,
                ),
            ) => {
                // The class and status only: GitHub's text never reaches a log.
                warn!(
                    "{repository}: GitHub refused the automatic re-run of run {run} ({:?}, status {:?}); no runner \
                     fault of {repository} is re-run until the config changes",
                    result.refusal, result.status
                );
                let refused = RerunRefused::new(result, time::now_ms() / 1000);
                Checked::Wait(self.faults.refuse(repository, revision, refused, now))
            }
            (ActionOutcome::Refused, Some(ActionRefusal::Unprocessable)) => {
                warn!("{repository}: GitHub cannot re-run run {run} (422); left to its owner");
                Checked::Settled
            }
            // Withdrawn by a reload, a conflict, a connection that failed, or an answer that does not say whether
            // GitHub acted: the next check reads the config and the run's attempt again before any send.
            _ => Checked::Failed,
        }
    }

    /// The jobs of `jobs` the runner failed, if every one that failed or was cancelled is one.
    async fn faulted(&self, jobs: &[AttemptJob]) -> Result<Vec<u64>, Unfit> {
        let mut faulted = Vec::new();
        for job in jobs {
            if job.status != "completed" {
                return Err(Unfit::Unfinished);
            }
            if matches!(
                job.conclusion.as_deref(),
                Some("success" | "skipped" | "neutral")
            ) {
                continue;
            }
            let summary = match self.store.job(job.id).await {
                Ok(Some(found)) => found.summary,
                Ok(None) => return Err(Unfit::Other(job.id)),
                Err(error) => {
                    warn!("cannot read job {} from the history: {error}", job.id);
                    return Err(Unfit::Unread);
                }
            };
            // The history may not have closed a job GitHub completed yet: its outcome comes once it has.
            if !summary.state.is_final() {
                return Err(Unfit::Unfinished);
            }
            if summary.outcome().kind != OutcomeKind::RunnerFault {
                return Err(Unfit::Other(job.id));
            }
            faulted.push(job.id);
        }
        Ok(faulted)
    }

    /// `read`, within the deadline and unless a stop comes first.
    async fn read<T>(
        &self,
        read: impl Future<Output = Result<T, ActionFailure>>,
    ) -> Result<T, ActionFailure> {
        tokio::select! {
            biased;
            () = self.control.stop_asked() => Err(ActionFailure::TimedOut),
            read = tokio::time::timeout(self.deadline, read) => read.unwrap_or(Err(ActionFailure::TimedOut)),
        }
    }
}

/// Why an attempt's failed jobs are not all runner faults now.
enum Unfit {
    /// A job has not completed, on GitHub or in the history.
    Unfinished,
    /// This job failed or was cancelled, and not by its runner, or not on this machine.
    Other(u64),
    /// The history could not be read.
    Unread,
}

/// What a read that failed comes to: the rate limit waits, anything else is a failed check.
fn waited(failure: ActionFailure, now: Instant) -> Checked {
    match classify(Err(failure)) {
        ActionAnswer::Refused {
            refusal: ActionRefusal::RateLimited,
            until,
            ..
        } => Checked::Wait(until_instant(until, now)),
        _ => Checked::Failed,
    }
}

/// When the rate limit that resets at `until`, seconds since the epoch, lets requests go again, from `now`; a minute
/// on when it does not say.
fn until_instant(until: Option<u64>, now: Instant) -> Instant {
    let epoch = time::now_ms() / 1000;
    let wait = until.map_or(POLL, |until| {
        Duration::from_secs(until.saturating_sub(epoch)).max(Duration::from_secs(1))
    });
    now + wait
}
