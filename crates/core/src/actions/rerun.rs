//! Re-runs a run whose runner failed one of its jobs, once per attempt, through the job actions' own path (ADR 0153):
//! GitHub's re-run of the run's failed jobs, without a preview, on the engine's own GitHub client. Off unless a
//! repository's `rerun_runner_faults` asks: a runner fault is read from the job's own output.
//!
//! runnerd decides to re-run a run only while GitHub says its current attempt is the one the fault happened in and it
//! completed as `failure`, every job of that attempt that failed or was cancelled is one the runner failed, and the
//! attempt is the first or the second; the permit reads the attempt and the run's conclusion once more right before
//! the send. The rule binds runnerd's decision, not the request: GitHub's re-run names no attempt, so one that a
//! person's re-run overtakes re-runs whatever attempt is current when GitHub handles it.
//!
//! Before the send, the attempt is claimed in the history, synced to disk; neither this daemon nor a restarted one
//! sends a claimed attempt again. The claim goes only when nothing reached GitHub: the permit stopped it, a reload
//! withdrew it before the permit, or GitHub refused it. A send GitHub did not answer, or that a reload stopped after
//! the permit let it go, keeps its claim; a read of the run says whether it moved, but not who moved it, so only an
//! accepted re-run is noted as runnerd's. Faults of a repository whose `rerun_runner_faults` or `[github] job_actions`
//! is off, or that the config no longer has, wait unread until a reload turns them on or the run is no longer
//! followed. A read GitHub refuses in a way asking again soon cannot change, such as a token without Actions: write,
//! holds the repository's re-runs for an hour or until the config changes, with one attention item; a rate limit
//! stops every run's reads until it resets. A read after an unanswered send does the same.
//!
//! It must not send an attempt it claimed before, decide on a run from its third attempt on, send a held repository's
//! re-run, send while the rate limit answered, read GitHub for a run more than once a minute, or write the history
//! beyond its claims and the note of a re-run GitHub accepted.

use std::time::Duration;

use tokio::time::Instant;

use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use super::faults::{Due, RunKey};
use super::flights::{Boarding, Issued, Landed, Origin, Withheld};
use super::{JobActions, classify, wait};
use crate::api::{
    ActionAnswer, ActionOutcome, ActionRefusal, ActionResult, ActionTarget, Event, JobAction,
    OutcomeKind, RerunRefused, RunSeen, Superseded,
};
use crate::github::{ActionFailure, AttemptJob};
use crate::store::Claim;
use crate::time;

/// How far back a start reads the history's faults: as long as a run is followed.
const LOOKBACK: Duration = super::faults::FOLLOWED;

/// How long until a run that has not completed, or a run busy with another action, is read again.
const POLL: Duration = Duration::from_secs(60);

/// How long until a check that failed is made again.
const BACKOFF: Duration = Duration::from_secs(120);

/// How long every read waits after a rate limit GitHub answered without saying when it resets.
const COOLDOWN: Duration = Duration::from_secs(60);

/// The attempts a fault is re-run from: the first and the second, so a run reaches at most its third by runnerd.
const ATTEMPTS_MAX: u32 = 2;

/// What one check of a run came to.
enum Checked {
    /// Nothing more to do for the attempt.
    Settled,
    /// Check again this long after the check ended, or sooner if another fault of the attempt comes; no failure.
    Wait(Duration),
    /// Check again at this time and no sooner: a hold or a rate limit.
    Hold(Instant),
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

    /// Checks every run whose check is due at `now`, one after the other. Each next check is timed from when its own
    /// ended, and a rate limit GitHub answered stops the rest of the pass.
    pub(super) async fn pass(&self, now: Instant) {
        // The pass's `now`, or later once its checks took time.
        let observed = || now.max(Instant::now());
        for due in self.faults.due(now) {
            if let Some(until) = self.faults.cooling(observed()) {
                self.faults.hold(&due, until);
                continue;
            }
            match self.check(&due, now).await {
                Checked::Settled => self.faults.settle(&due),
                Checked::Wait(after) => self.faults.wait(&due, observed() + after),
                Checked::Hold(at) => self.faults.hold(&due, at),
                Checked::Failed => {
                    if !self.faults.failed(&due, observed() + BACKOFF) {
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
            return Checked::Wait(POLL);
        }
        if let Some(held) = self.faults.held(repository, policy.revision(), now) {
            return Checked::Hold(held.until);
        }
        match self.read(self.github.run(repository, run)).await {
            Ok(read) if read.attempt != due.attempt => {
                info!(
                    "{repository}: not re-running run {run}: it is at attempt {} already",
                    read.attempt
                );
                return Checked::Settled;
            }
            Ok(read) if !read.completed => return Checked::Wait(POLL),
            Ok(read) if !read.failed => {
                info!("{repository}: not re-running run {run}: it did not complete as a failure");
                return Checked::Settled;
            }
            Ok(_) => {}
            Err(failure) => return self.refused(due, failure, policy.revision()),
        }
        let jobs = match self
            .read(self.github.attempt_jobs(repository, run, due.attempt))
            .await
        {
            Ok(jobs) => jobs,
            Err(failure) => return self.refused(due, failure, policy.revision()),
        };
        let faulted = match self.faulted(&jobs).await {
            Ok(faulted) => faulted,
            Err(Unfit::Unfinished) => return Checked::Wait(POLL),
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
        self.send_rerun(due, &faulted, policy.revision()).await
    }

    /// Sends the re-run of `due`'s run, whose runner failed `faulted`, under config revision `revision`, and what
    /// GitHub's answer comes to.
    async fn send_rerun(&self, due: &Due, faulted: &[u64], revision: u64) -> Checked {
        let RunKey { repository, run } = &due.key;
        match self
            .store
            .claim_rerun(repository, *run, due.attempt, time::now_ms())
            .await
        {
            Claim::Claimed => {}
            Claim::Taken => {
                info!(
                    "{repository}: not re-running run {run}: runnerd sent the re-run of attempt {} already",
                    due.attempt
                );
                return Checked::Settled;
            }
            Claim::Unstored => {
                warn!(
                    "{repository}: cannot claim the re-run of run {run} in the history; not sent"
                );
                return Checked::Failed;
            }
        }
        self.send_claimed(due, faulted, revision).await
    }

    /// Sends the re-run `send_rerun` claimed, and releases the claim if nothing of it reached GitHub.
    async fn send_claimed(&self, due: &Due, faulted: &[u64], revision: u64) -> Checked {
        let RunKey { repository, run } = &due.key;
        let release = || self.store.release_rerun(repository, *run, due.attempt);
        let issued = Issued {
            target: ActionTarget {
                action: JobAction::RerunFailed,
                job: due.job,
                name: String::new(),
                repository: repository.clone(),
                run: *run,
                attempt: Some(due.attempt),
            },
            revision,
            origin: Origin::Automatic,
        };
        let answer = match self.flights.board(&issued) {
            Boarding::Busy(_) => {
                release();
                return Checked::Wait(POLL);
            }
            Boarding::Follow(answer) => answer,
            Boarding::Lead { landing, answer } => {
                let landed = self.dispatch(&issued).await;
                landing.land(landed);
                answer
            }
        };
        let landed = wait(answer, &issued.target).await;
        // Only an answer that may mean GitHub acted keeps the claim.
        if !matches!(&landed, Landed::Done(result) if matches!(result.outcome, ActionOutcome::Accepted | ActionOutcome::Unknown))
        {
            release();
        }
        let result = match landed {
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
            Landed::Withheld(Withheld::NotFailed) => {
                info!("{repository}: not re-running run {run}: it is no longer a failed run");
                return Checked::Settled;
            }
            Landed::Withheld(Withheld::Unread(failure)) => {
                return self.refused(due, failure, revision);
            }
            Landed::Repreview { .. } => return Checked::Failed,
        };
        self.landed(due, faulted, &result, revision).await
    }

    /// What a read GitHub failed or refused comes to: a refusal asking again soon cannot change holds the repository,
    /// a rate limit stops every read until it resets, anything else is a failed check.
    fn refused(&self, due: &Due, failure: ActionFailure, revision: u64) -> Checked {
        let RunKey { repository, run } = &due.key;
        let answer = classify(Err(failure));
        match answer {
            ActionAnswer::Refused {
                refusal:
                    ActionRefusal::NoToken
                    | ActionRefusal::Unauthorized
                    | ActionRefusal::Forbidden
                    | ActionRefusal::NotFound,
                ..
            } => {
                let refused = RerunRefused::unread(repository, *run, answer, time::now_ms() / 1000);
                self.hold_repository(repository, *run, refused, revision)
            }
            ActionAnswer::Refused {
                refusal: ActionRefusal::RateLimited,
                until,
                ..
            } => self.cool(until),
            ActionAnswer::Refused {
                status: Some(429), ..
            } => self.cool(None),
            _ => Checked::Failed,
        }
    }

    /// Holds `repository`'s re-runs for the hour, as `refused` says.
    fn hold_repository(
        &self,
        repository: &str,
        run: u64,
        refused: RerunRefused,
        revision: u64,
    ) -> Checked {
        // The class and status only: GitHub's text never reaches a log.
        warn!(
            "{repository}: GitHub refused runnerd's automatic re-run of run {run} ({:?}, status {:?}); its runner \
             faults wait an hour",
            refused.refusal, refused.status
        );
        Checked::Hold(
            self.faults
                .refuse(repository, revision, refused, Instant::now()),
        )
    }

    /// Stops every read until the rate limit that resets at `until`, seconds since the epoch, does; [`COOLDOWN`] when
    /// it does not say.
    fn cool(&self, until: Option<u64>) -> Checked {
        let epoch = time::now_ms() / 1000;
        let wait = until.map_or(COOLDOWN, |until| {
            Duration::from_secs(until.saturating_sub(epoch)).max(Duration::from_secs(1))
        });
        let until = Instant::now() + wait;
        self.faults.cool(until);
        Checked::Hold(until)
    }

    /// What `result`, GitHub's answer to the re-run of `due`'s run, comes to.
    async fn landed(
        &self,
        due: &Due,
        faulted: &[u64],
        result: &ActionResult,
        revision: u64,
    ) -> Checked {
        let RunKey { repository, run } = &due.key;
        match (result.outcome, result.refusal) {
            (ActionOutcome::Accepted, _) => {
                self.noted(due, faulted);
                Checked::Settled
            }
            // GitHub did not say: the claim stays, so it is never sent again, as it may land yet. A read says whether
            // the run moved, not who moved it, so nothing is noted as runnerd's; a refused read holds or cools as
            // any other.
            (ActionOutcome::Unknown, _) => {
                match self.read(self.github.run(repository, *run)).await {
                    Ok(read) if read.attempt > due.attempt => info!(
                        "{repository}: run {run} moved to attempt {} after a re-run GitHub did not answer; it may be \
                         runnerd's or a person's",
                        read.attempt
                    ),
                    Ok(_) => warn!(
                        "{repository}: GitHub did not say whether it re-ran run {run} (status {:?}); runnerd does not \
                         ask again",
                        result.status
                    ),
                    Err(failure) => {
                        warn!(
                            "{repository}: GitHub did not say whether it re-ran run {run}, nor answered a read of it; \
                             runnerd does not ask again"
                        );
                        let _ = self.refused(due, failure, revision);
                    }
                }
                Checked::Settled
            }
            (ActionOutcome::Refused, Some(ActionRefusal::RateLimited)) => self.cool(result.until),
            (ActionOutcome::Refused, _) if result.status == Some(429) => self.cool(None),
            (
                ActionOutcome::Refused,
                Some(
                    ActionRefusal::NoToken
                    | ActionRefusal::Unauthorized
                    | ActionRefusal::Forbidden
                    | ActionRefusal::NotFound,
                ),
            ) => {
                let refused = RerunRefused::new(result, time::now_ms() / 1000);
                self.hold_repository(repository, *run, refused, revision)
            }
            (ActionOutcome::Refused, Some(ActionRefusal::Unprocessable)) => {
                warn!("{repository}: GitHub cannot re-run run {run} (422); left to its owner");
                Checked::Settled
            }
            // A reload withdrew it: the next check reads the config again.
            (ActionOutcome::Refused, Some(ActionRefusal::Withdrawn)) => Checked::Wait(POLL),
            // A conflict, or a connection that failed before anything was sent: the next check reads the config and
            // the run's attempt again before any send.
            _ => Checked::Failed,
        }
    }

    /// Notes that GitHub re-ran `due`'s run, whose runner failed `faulted`.
    fn noted(&self, due: &Due, faulted: &[u64]) {
        let RunKey { repository, run } = &due.key;
        info!(
            "{repository}: re-ran the failed jobs of run {run} automatically, as attempt {}: its runner failed job {}",
            due.attempt + 1,
            due.job
        );
        self.store.reran(faulted.to_vec(), time::now_ms());
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
