//! The job history the engine feeds: each runner's record, sampling and mirror lease, its log capture (which finds
//! the job it took), the queued jobs it may run, steps from GitHub, and closing the jobs of removed runners. It
//! must not start or remove containers or runners: that is `runners`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::warn;

use super::tick::CONCURRENT_REQUESTS;
use super::{Engine, Occupant};
use crate::docker::DockerApi;
use crate::github::{self, GithubApi, JobDetails};
use crate::observe::capture::{self, Captured, Parser};
use crate::observe::metrics::Tracked;
use crate::scheduler::SlotRef;
use crate::store::{LogPosition, RunnerRecord, Store};
use crate::time;

/// How long the history waits for GitHub to report a job completed after its container is gone, before it records
/// the job as lost. GitHub fails a job whose runner vanished after about 10 minutes.
pub(super) const LOST_AFTER: Duration = Duration::from_mins(15);

/// How long `release` waits for a runner's log capture to end once its container stopped.
const CAPTURE_DRAIN: Duration = Duration::from_secs(10);

/// How often the history drops what it no longer keeps.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// A job whose runner is gone, waiting for GitHub to report it completed.
#[derive(Debug, Clone)]
pub(super) struct Closing {
    repository: String,
    /// When the runner was found gone, on Tokio's clock, which tests can advance.
    since: tokio::time::Instant,
    /// The same, in milliseconds since the epoch.
    since_ms: u64,
}

impl Closing {
    pub(super) fn now(repository: &str) -> Closing {
        Closing {
            repository: repository.to_owned(),
            since: tokio::time::Instant::now(),
            since_ms: time::now_ms(),
        }
    }
}

/// A runner's log capture found its job.
#[derive(Debug)]
pub(super) struct Found {
    runner: String,
    job: u64,
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
    /// captures its job and log.
    pub(super) async fn observe(&mut self, slot: &SlotRef, occupant: &Occupant) {
        let state = match self.docker.inspect(&occupant.container).await {
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
        let position = match self.store.runner_job(&occupant.runner_name).await {
            Ok(Some(job)) => self
                .store
                .log_position(job)
                .await
                .unwrap_or(LogPosition { page: 1, offset: 0 }),
            _ => LogPosition { page: 1, offset: 0 },
        };
        let task = tokio::spawn(capture_job(
            self.docker.clone(),
            self.store.clone(),
            occupant.runner_name.clone(),
            occupant.container.clone(),
            position,
            self.config.history.log_max.0,
            self.found.0.clone(),
        ));
        if let Some(old) = self.captures.insert(occupant.runner_name.clone(), task) {
            old.abort();
        }
    }

    /// Notes the jobs runners took since the last call.
    pub(super) fn take_found(&mut self) {
        while let Ok(found) = self.found.1.try_recv() {
            self.queued_jobs.remove(&found.job);
            if let Some(occupant) = self
                .slots
                .values_mut()
                .flatten()
                .flatten()
                .find(|occupant| occupant.runner_name == found.runner)
            {
                occupant.job = Some(found.job);
            }
        }
    }

    /// Records the queued jobs the repositories' runners may run, and closes those that left the queue for another
    /// machine's runner or were cancelled; asks GitHub for the steps of running jobs and whether the jobs of removed
    /// runners completed.
    pub(super) async fn follow_jobs(&mut self) {
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        for repository in &self.config.repositories {
            let Some(state) = self.repositories.get(&repository.name) else {
                continue;
            };
            if !state.listed {
                continue;
            }
            for job in &state.queued {
                if crate::scheduler::matches(job, &repository.labels) && !taken.contains(&job.id) {
                    self.store.queued(&repository.name, job);
                    self.queued_jobs
                        .entry(job.id)
                        .or_insert_with(|| repository.name.clone());
                }
            }
        }
        let left: Vec<(String, u64)> = self
            .queued_jobs
            .iter()
            .filter(|(id, repository)| {
                !taken.contains(*id)
                    && self.repositories.get(*repository).is_some_and(|state| {
                        state.listed && !state.queued.iter().any(|job| job.id == **id)
                    })
            })
            .map(|(id, repository)| (repository.clone(), *id))
            .collect();
        for (repository, id, answer) in self.serving(read_jobs(self.github.clone(), left)).await {
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
                    self.queued_jobs.remove(&id);
                    self.store.elsewhere(details);
                }
                Err(error) => warn!("{repository}: cannot read job {id}: {error}"),
            }
        }
        let running: Vec<(String, u64)> = self
            .occupied()
            .filter_map(|(_, occupant)| Some((occupant.repository.clone(), occupant.job?)))
            .collect();
        for (repository, id, answer) in self.serving(read_jobs(self.github.clone(), running)).await
        {
            match answer {
                Ok(details) => self.store.details(details),
                Err(error) => warn!("{repository}: cannot read job {id}: {error}"),
            }
        }
        let closing: Vec<(String, u64)> = self
            .closing
            .iter()
            .map(|(id, closing)| (closing.repository.clone(), *id))
            .collect();
        for (repository, id, answer) in self.serving(read_jobs(self.github.clone(), closing)).await
        {
            let Some(Closing {
                since, since_ms, ..
            }) = self.closing.get(&id).cloned()
            else {
                continue;
            };
            match answer {
                Ok(details) if details.status == "completed" => {
                    self.closing.remove(&id);
                    self.store.details(details);
                }
                Ok(_) | Err(_) if since.elapsed() >= LOST_AFTER => {
                    warn!("{repository}: job {id} lost: GitHub did not report it completed");
                    self.closing.remove(&id);
                    self.store.lost(id, since_ms);
                }
                Ok(details) => self.store.details(details),
                Err(error) => warn!("{repository}: cannot read job {id}: {error}"),
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
        if let Some(mut task) = self.captures.remove(runner)
            && tokio::time::timeout(CAPTURE_DRAIN, &mut task)
                .await
                .is_err()
        {
            task.abort();
        }
        self.take_found();
    }

    /// Records a runner's removal and stops watching it; its job, if it took one, closes once GitHub reports it
    /// completed.
    pub(super) fn forget(&mut self, occupant: &Occupant) {
        let now = SystemTime::now();
        self.store
            .runner_removed(&occupant.runner_name, time::ms(now));
        self.tracked.send_modify(|tracked| {
            tracked.retain(|container| container.runner != occupant.runner_name);
        });
        self.leases.end(&occupant.runner_name, time::ms(now));
        let job = self
            .occupied()
            .find(|(_, candidate)| candidate.runner_name == occupant.runner_name)
            .and_then(|(_, candidate)| candidate.job)
            .or(occupant.job);
        if let Some(job) = job {
            self.closing.insert(job, Closing::now(&occupant.repository));
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
) {
    let mut output = match docker.exec(&container, capture::command(position)).await {
        Ok(output) => output,
        Err(error) => {
            warn!("{runner}: cannot capture its job and log: {error}");
            return;
        }
    };
    let mut parser = Parser::default();
    let mut job = None;
    // Bytes of the job's log stored or sent: past `max_bytes` the store keeps no more, so the capture stops.
    let mut sent = 0;
    while let Some(chunk) = output.next().await {
        // An error: the container stopped.
        let Ok(chunk) = chunk else { break };
        for captured in parser.feed(&chunk) {
            match captured {
                Captured::Job { id, run_id } => {
                    if job.is_none() {
                        job = Some(id);
                        store.claimed(id, run_id, &runner, time::now_ms());
                        let _ = found.send(Found {
                            runner: runner.clone(),
                            job: id,
                        });
                        // After a restart, the part stored before.
                        if let Ok(Some(stored)) = store.job(id).await {
                            if stored.summary.log.truncated {
                                return;
                            }
                            sent = stored.summary.log.bytes;
                        }
                    }
                }
                Captured::Lines { lines, position } => {
                    if let Some(job) = job {
                        sent += lines.iter().map(|line| line.len() as u64 + 1).sum::<u64>();
                        store.log(job, lines, position, max_bytes).await;
                        if sent > max_bytes {
                            return;
                        }
                    }
                }
            }
        }
    }
}
