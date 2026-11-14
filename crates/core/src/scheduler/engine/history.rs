//! The job history the engine feeds: each runner's record, sampling and mirror lease, its log capture (which finds
//! the job it took, and starts again when Docker breaks it off), the queued jobs it may run, steps from GitHub, and
//! closing the jobs of removed runners. It must not start or remove containers or runners: that is `runners`. Also
//! each repository's visibility, which pricing at GitHub's rates reads from the store (ADR 0072).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::ledger::{ReadFailure, Unanswered, Wait};
use super::tick::CONCURRENT_REQUESTS;
use super::{Engine, Meanwhile, Occupant, Slot, watches};
use crate::admission::Key;
use crate::docker::{
    self, DockerApi, RunnerState, is_declined, is_log_gone, is_not_found, is_not_running,
};
use crate::github::{self, GithubApi, JobDetails};
use crate::observe::capture::{self, Captured, Parser};
use crate::observe::metrics::Tracked;
use crate::scheduler::warm::Attempt;
use crate::scheduler::{Pool, SlotRef};
use crate::store::{LogPosition, RunnerRecord, Store};
use crate::sync::lock;
use crate::time;

/// How long the history waits for GitHub to report a job completed after its container is gone, before it records
/// the job as lost. GitHub fails a job whose runner vanished after about 10 minutes.
pub(super) const LOST_AFTER: Duration = Duration::from_mins(15);

/// How long a repository's visibility stands before GitHub is asked again.
const VISIBILITY_FOR: Duration = Duration::from_hours(24);

/// How long after a failed ask GitHub is asked a repository's visibility again; the store keeps the last answer.
const VISIBILITY_RETRY: Duration = Duration::from_hours(1);

/// How long `release` waits for a runner's log capture to end once its container stopped.
pub(super) const CAPTURE_DRAIN: Duration = Duration::from_secs(10);

/// How often the history drops what it no longer keeps.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// Where a capture starts when nothing of its job's log is stored.
const FIRST_PAGE: LogPosition = LogPosition { page: 1, offset: 0 };

/// How a runner's log capture ended.
#[derive(Debug)]
pub(super) enum Capture {
    /// Nothing is left to capture: the container stopped, or the job's log is stored whole or up to `log_max`.
    Done,
    /// Docker failed the exec or broke its output off, though the container may run on: `recapture` starts it
    /// again. `resume` is where the lines it stored end, if it stored any: the store may not have written them yet.
    Broken { resume: Option<LogPosition> },
}

/// A runner's log capture found its job.
#[derive(Debug)]
pub(super) struct Found {
    runner: String,
    job: u64,
    run_id: u64,
}

#[cfg(test)]
impl Found {
    /// That `runner` took `job`, of a run with the job's ID.
    pub(super) fn named(runner: String, job: u64) -> Found {
        Found {
            runner,
            job,
            run_id: job,
        }
    }
}

/// The jobs a tick asked GitHub about before its plan, for `follow_running`.
pub(super) struct Described {
    /// Those GitHub described, with their repositories.
    details: Vec<(String, u64, JobDetails)>,
    /// Every job asked about, answered or not: each is asked once a tick.
    asked: BTreeSet<u64>,
}

/// Asks GitHub for jobs, `CONCURRENT_REQUESTS` at a time; the answers come in the order asked. Owns what it needs,
/// so the engine answers commands while it runs.
async fn read_jobs<G: GithubApi>(
    github: Arc<G>,
    jobs: Vec<(String, u64)>,
) -> Vec<(String, u64, Result<JobDetails, github::Error>)> {
    let github = &*github;
    futures_util::stream::iter(jobs)
        .map(|(repository, id)| async move {
            let answer = github.job(&repository, id).await;
            (repository, id, answer)
        })
        .buffered(CONCURRENT_REQUESTS)
        .collect()
        .await
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Starts watching a runner's container: records it, samples it, leases its address for the mirror's log, and
    /// captures its job and log. If Docker fails to inspect it, `observe_again` tries on the next tick.
    pub(super) async fn observe(&mut self, slot: &SlotRef, occupant: &Occupant) {
        let inspected = self.docker.inspect(&occupant.container).await;
        self.watch_inspected(slot, occupant, inspected).await;
    }

    /// `observe`, from Docker's inspect of the runner's container, which a start's task made.
    pub(super) async fn watch_inspected(
        &mut self,
        slot: &SlotRef,
        occupant: &Occupant,
        inspected: Result<RunnerState, docker::Error>,
    ) {
        let state = match inspected {
            Ok(state) => state,
            Err(error) => {
                warn!("cannot inspect {}: {error}", occupant.runner_name);
                return;
            }
        };
        let started_at = state
            .started_at
            .unwrap_or_else(|| time::ms(occupant.started_at));
        self.store.runner_started(RunnerRecord {
            name: occupant.runner_name.clone(),
            repository: occupant.repository.clone(),
            profile: slot.profile.clone(),
            slot: slot.index + 1,
            container: occupant.container.clone(),
            started_at,
            memory_cap: state.memory,
            cpu_cap: state.cpus,
            admission: occupant.admission,
            purpose: occupant.purpose().map(super::Purpose::as_str),
        });
        if let Some(pid) = state.pid {
            self.tracked.send_modify(|tracked| {
                tracked.push(Tracked {
                    runner: occupant.runner_name.clone(),
                    container: occupant.container.clone(),
                    pid,
                });
            });
        }
        if let Some(address) = &state.address {
            self.leases
                .start(address, &occupant.runner_name, started_at);
        }
        // An adopted runner may have stored part of its log before the restart.
        let position = self.stored_position(&occupant.runner_name).await;
        self.capture(occupant, position);
    }

    /// Where the stored log of a runner's job ends.
    async fn stored_position(&self, runner: &str) -> LogPosition {
        match self.store.runner_job(runner).await {
            Ok(Some(job)) => self.store.log_position(job).await.unwrap_or(FIRST_PAGE),
            _ => FIRST_PAGE,
        }
    }

    /// Starts a runner's log capture from `position`, in place of any it had.
    fn capture(&mut self, occupant: &Occupant, position: LogPosition) {
        let task = tokio::spawn(capture_job(
            self.docker.clone(),
            self.store.clone(),
            occupant.runner_name.clone(),
            occupant.container.clone(),
            position,
            self.config.history.log_max.0,
            self.found.0.clone(),
        ));
        self.watches.start(&occupant.runner_name, task);
    }

    /// Starts again the log captures that broke off while their container runs, each after a wait that grows with
    /// its breaks (`capture_backoff`), from where its job's stored lines end. Without it, a runner whose capture
    /// broke would lose its job's name, log and record.
    pub(super) async fn recapture(&mut self) {
        let now = tokio::time::Instant::now();
        self.watches.settle(now).await;
        let due: Vec<(Occupant, Option<LogPosition>)> = self
            .occupied()
            .filter_map(|(_, occupant)| {
                let due = self.watches.due(&occupant.runner_name, now)?;
                Some((occupant.clone(), due.resume))
            })
            .collect();
        for (occupant, resume) in due {
            match self.docker.inspect(&occupant.container).await {
                Ok(state) if state.running => {
                    let stored = self.stored_position(&occupant.runner_name).await;
                    let position = watches::later(resume, Some(stored)).unwrap_or(stored);
                    info!(
                        "{}: capturing its job and log again from page {} at byte {}",
                        occupant.runner_name, position.page, position.offset
                    );
                    self.capture(&occupant, position);
                }
                // Its log ends where the capture broke off; `collect_finished` frees its slot.
                Ok(_) => {
                    self.watches.end_breaks(&occupant.runner_name);
                    self.claim_from_worker_log(&occupant).await;
                }
                Err(error) if is_not_found(&error) => {
                    self.watches.end_breaks(&occupant.runner_name);
                }
                Err(error) => warn!("cannot inspect {}: {error}", occupant.runner_name),
            }
        }
    }

    /// The job a runner took: its capture's last report may have named it after `occupant` was taken.
    pub(super) fn named_job(&self, occupant: &Occupant) -> Option<u64> {
        self.occupied()
            .find(|(_, candidate)| candidate.runner_name == occupant.runner_name)
            .and_then(|(_, candidate)| candidate.named_job())
            .or(occupant.named_job())
    }

    /// Claims the job a stopped runner's worker log names, if no capture read one: nothing else may link the two, and a
    /// job whose capture broke can end before a listing shows its runner.
    pub(super) async fn claim_from_worker_log(&mut self, occupant: &Occupant) {
        if self.named_job(occupant).is_some() {
            return;
        }
        let read = self
            .docker
            .files(&occupant.container, capture::DIAG, capture::WORKER_LOG)
            .await;
        let claim = match read {
            Ok(read) => read.files.iter().find_map(|file| {
                let job = capture::worker_job(&file.contents)?;
                Some((job, capture::worker_run(&file.contents)?))
            }),
            Err(error) if is_declined(&error) => None,
            Err(error) => {
                warn!("cannot read {}'s worker log: {error}", occupant.runner_name);
                None
            }
        };
        if let Some((job, run_id)) = claim {
            let runner = occupant.runner_name.clone();
            self.store.claimed(job, run_id, &runner, time::now_ms());
            self.note_found(&Found {
                runner,
                job,
                run_id,
            });
            // A left runner is in no slot, so no `forget` closes what the claim opened.
            let held = self
                .occupied()
                .any(|(_, candidate)| candidate.runner_name == occupant.runner_name);
            if !held {
                self.ledger.close(job, &occupant.repository);
                self.consume(&occupant.repository, job);
            }
        }
    }

    /// Watches the runners `observe` could not, as Docker failed to inspect their containers.
    pub(super) async fn observe_again(&mut self) {
        let unobserved: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .filter(|(_, occupant)| !self.watches.is_watched(&occupant.runner_name))
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        for (slot, occupant) in unobserved {
            self.observe(&slot, &occupant).await;
        }
    }

    /// Notes the jobs runners took since the last call.
    pub(super) fn take_found(&mut self) {
        while let Ok(found) = self.found.1.try_recv() {
            self.note_found(&found);
        }
    }

    /// Notes the job a runner's capture found.
    pub(super) fn note_found(&mut self, found: &Found) {
        self.ledger.stop_queued(found.job);
        // GitHub picks which job a runner takes, which may not be the one the plan started it for (ADR 0110).
        self.note_started(found.job);
        let Some((profile, occupant)) = self.slots.iter_mut().find_map(|(profile, slots)| {
            slots
                .iter_mut()
                .filter_map(Slot::occupant_mut)
                .find(|occupant| occupant.runner_name == found.runner)
                .map(|occupant| (profile, occupant))
        }) else {
            return;
        };
        // A capture started again reports the job it reported before.
        if occupant.runs(found.job) {
            return;
        }
        occupant.engagement.found(found.job, SystemTime::now());
        // A warm runner takes its job before a listing shows it queued: its run is seen here first. After a
        // restart, `recover` noted it with its jobs' queued times. Its job is its pool's.
        let pool = Pool {
            repository: occupant.repository.clone(),
            profile: profile.clone(),
        };
        let attempt = self.latest_attempt(found.run_id);
        self.note_job(&pool, attempt, found.job, None, time::now_ms());
    }

    /// Follows the jobs the last poll listed queued that no runner in `taken` has; the listing recorded them. A job
    /// whose runner went is followed as closing alone, though GitHub may list it queued a while longer.
    fn follow_listed(&mut self, taken: &BTreeSet<u64>) {
        for repository in &self.config.repositories {
            let Some(state) = self.repositories.listed(&repository.name) else {
                continue;
            };
            let Some(listed) = state.listing().listed() else {
                continue;
            };
            for job in listed {
                if crate::scheduler::matches(job, &repository.labels) && !taken.contains(&job.id) {
                    self.ledger.follow_queued(job.id, &repository.name);
                }
            }
        }
    }

    /// The runner of this machine GitHub says took a job, if it holds a slot and its log capture found no job.
    fn unclaimed_runner(&self, details: &JobDetails) -> Option<String> {
        let name = details.runner_name.as_ref()?;
        self.occupied()
            .find(|(_, occupant)| occupant.runner_name == *name && occupant.named_job().is_none())
            .map(|(_, occupant)| occupant.runner_name.clone())
    }

    /// Tells admission which job a runner runs as GitHub names it, which a runner may have taken before any
    /// listing did.
    fn name_job(&mut self, repository: &str, details: &JobDetails) {
        let key = Key {
            repository: repository.to_owned(),
            workflow: details.workflow_name.clone(),
            name: details.name.clone(),
        };
        for occupant in self
            .slots
            .values_mut()
            .flatten()
            .filter_map(Slot::occupant_mut)
        {
            if occupant.runs(details.id) {
                occupant.engagement.keyed(key.clone());
            }
        }
    }

    /// Asks GitHub each served repository's visibility, at start and once a day after, in a task of its own that
    /// stores the answers: a slow or hanging GitHub never holds up a tick, and pricing reads what is stored meanwhile.
    /// One ask at a time, `CONCURRENT_REQUESTS` requests at once.
    pub(super) fn follow_visibility(&mut self) {
        if self
            .visibility_asking
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let now = SystemTime::now();
        let due: Vec<String> = {
            let due_at = lock(&self.visibility_due);
            self.config
                .repository_names()
                .into_iter()
                .filter(|repository| due_at.get(repository).is_none_or(|due| now >= *due))
                .collect()
        };
        if due.is_empty() {
            return;
        }
        let (github, store, due_at) = (
            self.github.clone(),
            self.store.clone(),
            self.visibility_due.clone(),
        );
        self.visibility_asking = Some(tokio::spawn(async move {
            let answers: Vec<_> = futures_util::stream::iter(due)
                .map(|repository| {
                    let github = github.clone();
                    async move {
                        let answer = github.repository(&repository).await;
                        (repository, answer)
                    }
                })
                .buffer_unordered(CONCURRENT_REQUESTS)
                .collect()
                .await;
            for (repository, answer) in answers {
                let next = match answer {
                    Ok(details) => {
                        store.visibility(repository.clone(), details.visibility, time::ms(now));
                        VISIBILITY_FOR
                    }
                    Err(error) => {
                        warn!("{repository}: cannot read whether it is public: {error}");
                        VISIBILITY_RETRY
                    }
                };
                lock(&due_at).insert(repository, now + next);
            }
        }));
    }

    /// Follows the queued jobs the repositories' runners may run, and closes those that left the queue for another
    /// machine's runner or were cancelled; asks GitHub for the details of the running jobs admission cannot tell
    /// apart yet, which the plan needs. The other running jobs are read once the tick's starts are done
    /// (`follow_running`): reading each running job, a few requests at a time, held every start back.
    pub(super) async fn follow_jobs(&mut self) -> Described {
        let taken = self.taken();
        self.follow_listed(&taken);
        // A repository a reload removed lists nothing: GitHub says what became of its jobs. The clock counts failed
        // reads in a row: a job not read this tick starts it over (ADR 0109).
        let repositories = &self.repositories;
        let left = self.ledger.queued_to_read(|id, repository| {
            !taken.contains(&id)
                && repositories
                    .listed(repository)
                    .is_none_or(|state| state.listing().no_longer_shows(id))
        });
        for (repository, id, answer) in self
            .serving(read_jobs(self.github.clone(), left), Meanwhile::Turnover)
            .await
        {
            // One of this machine's runners took it, and its log capture has not said so, as when Docker failed it:
            // the job is that runner's.
            let unclaimed = match &answer {
                Ok(details) if details.status != "queued" && details.status != "waiting" => self
                    .unclaimed_runner(details)
                    .map(|runner| (runner, details.run_id)),
                _ => None,
            };
            if let Some((runner, run_id)) = unclaimed {
                self.store.claimed(id, run_id, &runner, time::now_ms());
                let _ = self.found.0.send(Found {
                    runner,
                    job: id,
                    run_id,
                });
                continue;
            }
            if answer.is_ok() {
                self.ledger.answered(id);
            }
            match answer {
                // Still queued: the listing lags.
                Ok(details) if details.status == "queued" || details.status == "waiting" => {}
                // One of this machine's runners: its log capture reports it. If it never did by the time the job
                // completed, the history knows no more of it than of another machine's job.
                Ok(details)
                    if details.status != "completed"
                        && details
                            .runner_name
                            .as_ref()
                            .is_some_and(|name| name.starts_with(&self.runner_prefix)) => {}
                Ok(details) => {
                    self.ledger.stop_queued(id);
                    self.store.elsewhere(details);
                }
                Err(error) => self.unanswered(&repository, id, &error),
            }
        }
        // The jobs claimed above, so their details are read with the running ones'. A job a listing showed is named
        // by the history's record of it; only those no listing showed need GitHub's details before the plan.
        self.take_found();
        self.find_keys().await;
        let unnamed: Vec<(String, u64)> = self
            .occupied()
            .filter(|(_, occupant)| !occupant.has_key())
            .filter_map(|(_, occupant)| Some((occupant.repository.clone(), occupant.named_job()?)))
            .collect();
        self.describe(unnamed).await
    }

    /// Stops following a job that left the queue once GitHub says it is gone, as when its run or repository was deleted
    /// or the token can no longer see it, or fails every read of it for `LOST_AFTER`; a used-up rate limit is no
    /// read, and starts the clock over. None of this machine's runners took it, so its record closes as run elsewhere (ADR 0109).
    fn unanswered(&mut self, repository: &str, id: u64, error: &github::Error) {
        let failure = if matches!(error, github::Error::RateLimited { .. }) {
            ReadFailure::RateLimited
        } else if error.is_gone() {
            ReadFailure::Gone
        } else {
            ReadFailure::Failed
        };
        match self.ledger.read_failed(id, failure) {
            Unanswered::NotQueued => {}
            Unanswered::Waits => warn!("{repository}: cannot read job {id}: {error}"),
            Unanswered::GivesUp => {
                warn!("{repository}: stops following job {id}, recorded as run elsewhere: {error}");
                self.store.unanswered(id, time::now_ms());
            }
        }
    }

    /// After the tick's starts: asks GitHub for the steps of the running jobs `follow_jobs` did not read, but only
    /// once every `poll_interval` (`RunningReads`), since the tick's listing began at `anchor`; and for the jobs of
    /// ended runners whose build cache uploads wait, every tick, since an end is read once and a plan waits for
    /// nothing here. Decides the writes of the jobs described, closes the build caches of ended runners and asks
    /// whether the jobs of removed runners completed, every tick.
    pub(super) async fn follow_running(&mut self, before: Described, anchor: tokio::time::Instant) {
        let interval = self.governor.reads_every(self.config.host.poll_interval);
        let running = if self.running_reads.due(anchor, interval) {
            self.running_reads.read(anchor);
            self.occupied()
                .filter_map(|(_, occupant)| {
                    Some((occupant.repository.clone(), occupant.named_job()?))
                })
                .collect()
        } else {
            Vec::new()
        };
        let ended = self
            .ended_uploads
            .values()
            .map(|ended| (ended.repository.clone(), ended.job));
        let reads: Vec<(String, u64)> = running
            .into_iter()
            .chain(ended)
            .filter(|(_, id)| !before.asked.contains(id))
            .collect();
        let mut described = before.details;
        described.extend(self.describe(reads).await.details);
        // Jobs recorded under GitHub's name for their run, as their workflow's could not be read then.
        for named in self.github.named_runs().await {
            self.store.workflow_named(named);
        }
        let undecided = self.undecided(&described);
        self.decide_writes(undecided).await;
        self.detach_ended();
        self.follow_closing().await;
    }

    /// Asks GitHub for the details of `jobs`, running or ended: names their runners' jobs for admission, places them
    /// in their runs and records them.
    async fn describe(&mut self, jobs: Vec<(String, u64)>) -> Described {
        let asked = jobs.iter().map(|(_, id)| *id).collect();
        let mut described = Vec::new();
        for (repository, id, answer) in self
            .serving(read_jobs(self.github.clone(), jobs), Meanwhile::Turnover)
            .await
        {
            match answer {
                Ok(details) => {
                    // The attempt and workflow of a run first seen through a job a runner took.
                    if let Some(pool) = self
                        .config
                        .pool(&repository, &details.labels)
                        .map(Pool::from)
                    {
                        self.place_job(
                            &pool,
                            Attempt {
                                run_id: details.run_id,
                                run_attempt: details.run_attempt,
                            },
                            id,
                            details.workflow_name.as_deref(),
                        );
                    }
                    self.note_workflow(details.run_id, details.workflow_name.as_deref());
                    self.name_job(&repository, &details);
                    described.push((repository, id, details.clone()));
                    self.store.details(details);
                }
                Err(error) => warn!("{repository}: cannot read job {id}: {error}"),
            }
        }
        Described {
            details: described,
            asked,
        }
    }

    /// Asks GitHub whether the jobs of removed runners completed, and records them lost once it says nothing of them
    /// for `LOST_AFTER`.
    async fn follow_closing(&mut self) {
        let closing = self.ledger.closing();
        for (repository, id, answer) in self
            .serving(read_jobs(self.github.clone(), closing), Meanwhile::Turnover)
            .await
        {
            // A pass may have closed it again meanwhile, starting its wait over.
            let Some(wait) = self.ledger.wait(id) else {
                continue;
            };
            match (answer, wait) {
                (Ok(details), _) if details.status == "completed" => {
                    self.ledger.closed(id);
                    self.store.details(details);
                }
                (Ok(_) | Err(_), Wait::Over { since_ms }) => {
                    warn!("{repository}: job {id} lost: GitHub did not report it completed");
                    self.ledger.closed(id);
                    self.store.lost(id, since_ms);
                }
                (Ok(details), Wait::Waiting) => self.store.details(details),
                (Err(error), Wait::Waiting) => warn!("{repository}: cannot read job {id}: {error}"),
            }
        }
    }

    pub(super) fn prune(&mut self) {
        let now = SystemTime::now();
        if self.last_prune.is_some_and(|last| {
            now.duration_since(last)
                .is_ok_and(|since| since < PRUNE_EVERY)
        }) {
            return;
        }
        self.last_prune = Some(now);
        // Runs on its own; the tick does not wait for it.
        drop(self.store.prune(time::ms(now), &self.config.history));
    }

    /// Waits for a runner's log capture to end, which it does once the container stopped; stops it after
    /// `CAPTURE_DRAIN`.
    pub(super) async fn end_capture(&mut self, runner: &str) {
        self.take_found();
        if let Some(mut task) = self.watches.take_capture(runner)
            && tokio::time::timeout(CAPTURE_DRAIN, &mut task)
                .await
                .is_err()
        {
            task.abort();
        }
        self.take_found();
    }

    /// Records a runner's removal and what the build cache served it, and stops watching it; its job, if it took
    /// one, closes once GitHub reports it completed.
    pub(super) fn forget(&mut self, occupant: &Occupant) {
        let job = self.named_job(occupant);
        // Before the record of its removal, which the job's record then shows.
        self.detach_build_cache(occupant, job);
        let now = SystemTime::now();
        self.store
            .runner_removed(&occupant.runner_name, time::ms(now));
        self.watches.forget(&occupant.runner_name);
        self.tracked.send_modify(|tracked| {
            tracked.retain(|container| container.runner != occupant.runner_name);
        });
        self.leases.end(&occupant.runner_name, time::ms(now));
        if let Some(job) = job {
            self.ledger.close(job, &occupant.repository);
            self.consume(&occupant.repository, job);
        }
    }
}

/// Runs a runner's log capture until its container stops: reports the job it took, and stores the job's log.
async fn capture_job<D: DockerApi>(
    docker: Arc<D>,
    store: Store,
    runner: String,
    container: String,
    position: LogPosition,
    max_bytes: u64,
    found: mpsc::UnboundedSender<Found>,
) -> Capture {
    let mut output = match docker.exec(&container, capture::command(position)).await {
        Ok(output) => output,
        // It stopped or is going since it was started or adopted: its release reads its log (`recapture`).
        Err(error) if is_not_running(&error) || is_log_gone(&error) => {
            debug!("{runner}: no capture, as its container stopped: {error}");
            return Capture::Broken { resume: None };
        }
        Err(error) => {
            warn!("{runner}: cannot capture its job and log: {error}");
            return Capture::Broken { resume: None };
        }
    };
    let mut parser = Parser::default();
    let mut job = None;
    // Bytes of the job's log stored or sent: past `max_bytes` the store keeps no more, so the capture stops.
    let mut sent = 0;
    let mut resume = None;
    while let Some(chunk) = output.next().await {
        // The container stopped, or Docker dropped the exec while it runs: `recapture` tells them apart.
        let Ok(chunk) = chunk else {
            return Capture::Broken { resume };
        };
        for captured in parser.feed(&chunk) {
            match captured {
                Captured::Job { id, run_id } => {
                    if job.is_none() {
                        job = Some(id);
                        store.claimed(id, run_id, &runner, time::now_ms());
                        let _ = found.send(Found {
                            runner: runner.clone(),
                            job: id,
                            run_id,
                        });
                        // After a restart, the part stored before.
                        if let Ok(Some(stored)) = store.job(id).await {
                            if stored.summary.log.truncated {
                                return Capture::Done;
                            }
                            sent = stored.summary.log.bytes;
                        }
                    }
                }
                Captured::Lines { lines, position } => {
                    if let Some(job) = job {
                        sent += lines.iter().map(|line| line.len() as u64 + 1).sum::<u64>();
                        store.log(job, lines, position, max_bytes).await;
                        resume = Some(position);
                        if sent > max_bytes {
                            return Capture::Done;
                        }
                    }
                }
            }
        }
    }
    Capture::Done
}
