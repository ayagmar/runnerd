//! A slot's runner once started, to its removal: stops idle or forced ones, frees the slots of ended containers and
//! holds back a pool whose runners fail to start or keep ending without a job (ADR 0104); starts go through `start`. It
//! must not decide which slots start or stop: `tick` does, through `plan`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use tracing::{info, warn};

use super::history::{CAPTURE_DRAIN, Capture};
use super::runner_exit::{Exit, Logged, TOOK_JOB};
use super::{Engine, Occupant, STOP_DEADLINE, Shutdown, Slot};
use crate::api::{Event, HoldCause, RunnerFailure};
use crate::cache;
use crate::docker::{self, DockerApi, is_being_removed, is_log_gone, is_not_found};
use crate::github::{Deleted, GithubApi, Runner};
use crate::record;
use crate::scheduler::{SlotRef, StopReason};
use crate::sync::lock;

/// The runner container's label with the labels its runner registered with, comma-separated as GitHub's runner
/// takes them, so a restart knows them.
pub(super) const LABELS: &str = "runnerd.labels";

/// The runner container's label with the image it runs, as the config named it, so a restart knows it.
pub(super) const IMAGE: &str = "runnerd.image";

/// The runner container's label with the release of runnerd that started it, whose container spec (mounts,
/// environment, entrypoint) it runs: a restart after an install replaces it once it is idle.
pub(super) const SPEC: &str = "runnerd.spec";

/// This release's `SPEC`.
pub(super) const THIS_SPEC: &str = env!("CARGO_PKG_VERSION");

/// What a runner logs as its job ends, whatever the result. GitHub drops an ephemeral runner's registration with its
/// job, so one that logged this needs no delete.
const JOB_ENDED: &str = " completed with result: ";

/// How many of a runner's last lines a warning shows, and how much of them.
const LAST_SAID_LINES: usize = 3;
const LAST_SAID_CHARS: usize = 300;

/// What a release's first step found of a runner that ended.
#[derive(Debug, Default)]
pub(super) struct Collected {
    /// Its log, if Docker read it.
    pub(super) logs: Option<String>,
    /// Its capture broke off, or was still reading when its time ran out: no tick saw that, as the runner goes first.
    capture_unread: bool,
}

/// What became of a stopped runner's container at its release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Removal {
    /// It is gone, or stays as a record.
    Done,
    /// Docker is removing it already, which can still fail: `leftovers` follows it (ADR 0105).
    UnderWay,
}

/// A slot's starts that failed preparing its caches, in a row, so one slot that stays bad refuses no other: spaced out without holding its pools back, whose
/// wait for memory or CPU a hold would end (ADR 0096, 0098).
#[derive(Debug)]
pub(super) struct PrepareFailures {
    failures: u32,
    /// No start in the slot prepares its caches before this.
    pub(super) retry_at: SystemTime,
}

impl Default for PrepareFailures {
    fn default() -> PrepareFailures {
        PrepareFailures {
            failures: 0,
            retry_at: SystemTime::UNIX_EPOCH,
        }
    }
}

/// The last lines a runner logged, on one line, for a warning about one that took no job: its log goes with its
/// container, so this is what is left to say why. The lines `run.sh` and its helper print on every exit say nothing
/// of why, so they are left out. A runner that took no job logged no job's output.
fn last_said(logs: Option<&str>) -> String {
    let mut said: Vec<&str> = logs
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("Runner listener exit with")
                && *line != "Exiting runner..."
        })
        .rev()
        .take(LAST_SAID_LINES)
        .collect();
    if said.is_empty() {
        return "nothing".to_owned();
    }
    said.reverse();
    said.join(" / ").chars().take(LAST_SAID_CHARS).collect()
}

/// Where the next backoff of a repository whose runners keep failing ends.
pub(super) fn backoff(failures: u32) -> Duration {
    Duration::from_secs(30 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(600))
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Frees the slots whose container stopped: the runner took its job and exited, or it never started. First
    /// removes again the containers recovery failed to remove, and keeps or removes those it could not tell about
    /// once their backoff is over. It inspects every runner but those Docker's events said stopped while runnerd runs
    /// on, which are a pass's. Forgets the background deletes that finished, so they do not pile up.
    pub(super) async fn collect_finished(&mut self) {
        while lock(&self.deletes).try_join_next().is_some() {}
        self.exits.inspecting_all();
        self.released.clear();
        let now = tokio::time::Instant::now();
        for unread in self.leftovers.take_due(now) {
            self.decide_left(unread).await;
        }
        // Those the reads above just left to remove too.
        for left in self.leftovers.to_remove() {
            match self.docker.remove(&left.id).await {
                // Asked again until Docker says it is gone, or why it is not.
                Err(error) if is_being_removed(&error) => {}
                Err(error) if !is_not_found(&error) => {
                    warn!("cannot remove {}: {error}", left.name);
                }
                _ => {
                    info!("removed {}", left.name);
                    self.leftovers.removed(&left.id);
                }
            }
        }
        let occupied: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        for (slot, occupant) in occupied {
            // Docker said it stopped: a pass reaps it while this tick waits for GitHub, and starts the next runner
            // into its room from the last listing, rather than after this tick's (ADR 0095).
            if self.control.mode() == Shutdown::No && self.exits.noted_stopped(&occupant.container)
            {
                continue;
            }
            if self.reap(&slot, &occupant).await {
                self.released.insert(occupant.container);
            }
        }
    }

    /// Releases a slot's runner if Docker says its container stopped or is gone; returns whether the slot is free
    /// now. Docker's say, not an event's: an event only says when to ask (ADR 0095).
    pub(super) async fn reap(&mut self, slot: &SlotRef, occupant: &Occupant) -> bool {
        if !self.ended(occupant).await {
            return false;
        }
        self.release(slot, occupant).await;
        !self.holds_runner(slot)
    }

    /// Whether Docker says a runner's container stopped or is gone; notes a kill for memory and logs the end.
    pub(super) async fn ended(&mut self, occupant: &Occupant) -> bool {
        let state = match self.docker.inspect(&occupant.container).await {
            Ok(state) => Some(state),
            Err(error) if is_not_found(&error) => None,
            // Its slot stays its until Docker says it stopped (ADR 0076); a stop leaves it to the next start.
            Err(error) => {
                warn!("cannot inspect {}: {error}", occupant.runner_name);
                self.docker_failed(&error);
                return false;
            }
        };
        // A kill in the last interval before it stopped, which no sample of its cgroup saw.
        if state.as_ref().is_some_and(|state| state.oom_killed) {
            self.store.runner_oom_killed(&occupant.runner_name);
        }
        if state.as_ref().is_some_and(|state| state.running) {
            return false;
        }
        // How long it took to notice: the pacing of slots between jobs (ADR 0095).
        let noticed = state
            .and_then(|state| state.finished_at)
            .map(|stopped| {
                let after = Duration::from_millis(crate::time::now_ms().saturating_sub(stopped));
                format!(
                    " {} after its container stopped",
                    super::tick::seconds(after)
                )
            })
            .unwrap_or_default();
        info!(
            "{}: {} ended{noticed}",
            occupant.repository, occupant.runner_name
        );
        true
    }

    /// Removes the container of a slot's runner that ended and its registration, then frees the slot. If its runner
    /// took no job and never connected, or is the second in a row that connected and left, its pool's next runners are
    /// held back for a while. A container Docker fails to remove keeps
    /// its slot (ADR 0076): the next tick tries again, and a stop leaves it to the next start. In steps a pass runs
    /// for several runners at once: `drain`, `record`, `remove`, `released`.
    async fn release(&mut self, slot: &SlotRef, occupant: &Occupant) {
        let drained = self.drain(occupant).await;
        let recorded = self.record(slot, occupant, &drained).await;
        let removed = self
            .remove(occupant, recorded, drained.logs.as_deref())
            .await;
        self.released(slot, occupant, drained.logs.as_deref(), removed);
    }

    /// A release's first step, on its own: reads the log of a runner that ended, then lets its capture end, which it
    /// does with the container, within `CAPTURE_DRAIN`.
    pub(super) fn drain(
        &mut self,
        occupant: &Occupant,
    ) -> impl Future<Output = Collected> + Send + 'static {
        self.take_found();
        let capture = self.watches.take_capture(&occupant.runner_name);
        let docker = self.docker.clone();
        let (container, runner) = (occupant.container.clone(), occupant.runner_name.clone());
        async move {
            // The runner logs each job it takes and how it ended; its exit code says nothing (run.sh exits 0 when it
            // fails to start).
            let logs = match docker.logs(&container).await {
                Ok(logs) => Some(logs),
                // Someone removed it: nothing is wrong, and its log went with it.
                Err(error) if is_log_gone(&error) => None,
                Err(error) => {
                    warn!("cannot read the log of {runner}: {error}");
                    None
                }
            };
            // The capture ends with the container; its last lines must be stored before the job's record closes.
            let capture_unread = match capture {
                Some(mut task) => match tokio::time::timeout(CAPTURE_DRAIN, &mut task).await {
                    Ok(Ok(Capture::Done)) => false,
                    Ok(_) => true,
                    Err(_) => {
                        task.abort();
                        true
                    }
                },
                None => false,
            };
            Collected {
                logs,
                capture_unread,
            }
        }
    }

    /// A release's second step: whether a failed job's container is kept as its record, which is then removed in the
    /// background.
    pub(super) async fn record(
        &mut self,
        slot: &SlotRef,
        occupant: &Occupant,
        drained: &Collected,
    ) -> bool {
        self.take_found();
        // A pass can reap it before `recapture` looks: what the capture left unread goes with the container.
        if self.watches.end_breaks(&occupant.runner_name) || drained.capture_unread {
            self.claim_from_worker_log(occupant).await;
        }
        self.count_mirror_bytes().await;
        drained.logs.as_deref().is_some_and(record::failed)
            && self.begin_record(slot, occupant).await
    }

    /// Waits for the background deletes of released runners' registrations, up to `STOP_DEADLINE`: the daemon's exit
    /// would drop them, leaving each registration on GitHub, offline, until the next start's runners listing.
    pub(super) async fn finish_deletes(&mut self) {
        let mut deletes = std::mem::take(&mut *lock(&self.deletes));
        let finished = async { while deletes.join_next().await.is_some() {} };
        if tokio::time::timeout(STOP_DEADLINE, finished).await.is_err() {
            warn!(
                "registration deletes still unanswered after {}s: {}; the next start deletes those runners",
                STOP_DEADLINE.as_secs(),
                deletes.len()
            );
        }
    }

    /// A release's third step, on its own: removes the container, unless it is `recorded`. Then deletes the runner's
    /// registration in the background, unless its `logs` say its job ended: the slot does not wait for GitHub, as a
    /// registration with no container takes no job, and the next runners listing deletes one this leaves.
    pub(super) fn remove(
        &self,
        occupant: &Occupant,
        recorded: bool,
        logs: Option<&str>,
    ) -> impl Future<Output = Result<Removal, docker::Error>> + Send + 'static {
        let (docker, github) = (self.docker.clone(), self.github.clone());
        let (container, runner) = (occupant.container.clone(), occupant.runner_name.clone());
        let (repository, runner_id) = (occupant.repository.clone(), occupant.runner_id);
        let deletes = self.deletes.clone();
        let job_ended = logs.is_some_and(|logs| logs.lines().any(|line| line.contains(JOB_ENDED)));
        async move {
            let mut removal = Removal::Done;
            if !recorded {
                match docker.remove(&container).await {
                    // It stopped, so nothing uses its volumes: the slot need not wait for that removal to end.
                    Err(error) if is_being_removed(&error) => removal = Removal::UnderWay,
                    Err(error) if !is_not_found(&error) => return Err(error),
                    _ => {}
                }
            }
            if !job_ended {
                // Busy: left to the next poll's cleanup of runners without a container.
                lock(&deletes).spawn(async move {
                    if let Err(error) = github.delete_runner(&repository, runner_id).await {
                        warn!("cannot delete {runner}: {error}");
                    }
                });
            }
            Ok(removal)
        }
    }

    /// A release's last step, once its container's removal `removed` it: frees the slot, or keeps it if Docker failed
    /// to remove the container.
    pub(super) fn released(
        &mut self,
        slot: &SlotRef,
        occupant: &Occupant,
        logs: Option<&str>,
        removed: Result<Removal, docker::Error>,
    ) {
        match removed {
            Ok(Removal::Done) => {}
            Ok(Removal::UnderWay) => {
                self.leftovers
                    .removing(occupant.container.clone(), occupant.runner_name.clone());
            }
            Err(error) => {
                warn!("cannot remove {}: {error}", occupant.runner_name);
                self.docker_failed(&error);
                return;
            }
        }
        let logged = Logged::read(logs, occupant.started_at);
        // Its capture may have named its job by now.
        let job = self
            .occupant(slot)
            .and_then(Occupant::named_job)
            .or(occupant.named_job());
        // GitHub may hold its registration busy a while yet, also for a job no poll or capture saw it take: the next
        // listings delete it then, without a warning.
        self.registrations
            .ended_here(occupant.runner_id, &occupant.repository);
        self.forget(occupant);
        let exit = Exit::Released {
            logged,
            listed_busy: occupant.listed_busy(),
            job,
        };
        self.note_exit(&occupant.repository, &slot.profile, &exit);
        let beside: BTreeSet<String> = self
            .occupied()
            .filter(|(other, runner)| {
                other.profile == slot.profile
                    && runner.repository == occupant.repository
                    && runner.runner_name != occupant.runner_name
            })
            .map(|(_, runner)| runner.runner_name.clone())
            .collect();
        if let Some(health) = self
            .repositories
            .listed_mut(&occupant.repository)
            .map(|state| state.health_mut(&slot.profile))
        {
            let cause = health.runner_left(logged, &occupant.runner_name, beside);
            let error = match logged {
                Logged::TookJob | Logged::Unread => None,
                // It started and connected, so it says nothing of the next one's start: a job cancelled before it
                // began, a warm runner whose container went once (a restart of Docker, the runner removed on GitHub).
                // Holding the pool would clear its waits and served mark (ADR 0098) for nothing. The second in a row
                // is a fault that would restart one every few minutes, unless it was already up when the first left:
                // a cancelled run takes several at once (ADR 0104).
                Logged::NoJob { connected: true } => {
                    warn!(
                        "{}: {} ended without a job after waiting for one; it last said: {}",
                        occupant.repository,
                        occupant.runner_name,
                        last_said(logs)
                    );
                    Some("its runners end without a job after waiting for one".to_owned())
                }
                Logged::NoJob { connected: false } => {
                    warn!(
                        "{}: {} ended without taking a job; it last said: {}",
                        occupant.repository,
                        occupant.runner_name,
                        last_said(logs)
                    );
                    Some(format!(
                        "{} ended without taking a job",
                        occupant.runner_name
                    ))
                }
            };
            if let Some((cause, error)) = cause.zip(error) {
                self.hold(&occupant.repository, &slot.profile, cause, error);
            }
        }
        self.vacate(slot, &exit);
    }

    /// Notes what a runner's end says of its repository: its job ended, or it may have taken one the last poll listed
    /// queued.
    fn note_exit(&mut self, repository: &str, profile: &str, exit: &Exit) {
        if exit.job_ended() {
            self.note_finished(repository);
        }
        if exit.spends() {
            self.spend(repository, profile);
        }
    }

    /// Frees a slot whose runner is gone. Its runner may have grown the shared caches, and if `exit` says it ran a job
    /// in slot 1, its profile's layers: both are checked once idle. A killed job's partial writes count too: they stay
    /// in slot 1 and would reach the snapshot on any later reseed, like those of a job cancelled or out of memory. A
    /// warm runner that leaves slot 1 without a job changed nothing there.
    fn vacate(&mut self, slot: &SlotRef, exit: &Exit) {
        let ran = exit.ran();
        let left = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
            .and_then(Slot::vacate);
        // Its registration's delete shows in the runners listing.
        if let Some(occupant) = &left {
            self.owe_runners_listing(&occupant.repository);
        }
        // Its layer holds what that job cached. A runner that ran a job nobody named leaves the layer to a job
        // unknown, and one that ran none leaves it as it was.
        match left.and_then(|occupant| occupant.key().cloned()) {
            Some(key) => {
                self.ran.insert(slot.clone(), key);
            }
            None if ran => {
                self.ran.remove(slot);
            }
            None => {}
        }
        if slot.index == 0 && ran {
            self.upkeep.slot_one_ran(&slot.profile);
        }
        self.trim_due = true;
        self.drop_closed_slots();
    }

    /// A runner container's environment: its slot's share of what the slots' Docker Engines may keep, which its
    /// image's job cleanup holds it to, once the caches' check has measured Docker's disk.
    pub(super) fn runner_env(&self) -> Vec<String> {
        self.docker_disk
            .borrow()
            .map(|disk| {
                let slots = self
                    .config
                    .profiles
                    .values()
                    .map(|p| u64::from(p.slots))
                    .sum();
                vec![format!(
                    "RUNNERD_DOCKER_BUDGET_GB={}",
                    cache::docker_budget_gib(disk, slots)
                )]
            })
            .unwrap_or_default()
    }

    /// Removes a slot's runner, busy or not, and kills its job. Nothing once Docker stopped answering a stop: the
    /// next start finds the runners as the stop left them.
    pub(super) async fn kill(&mut self, slot: &SlotRef) {
        if self.control.docker_gone() {
            return;
        }
        let Some(occupant) = self
            .slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .and_then(Slot::occupant)
            .cloned()
        else {
            return;
        };
        let what = if occupant.engaged() {
            "killing"
        } else {
            "removing idle"
        };
        info!("{}: {what} {}", occupant.repository, occupant.runner_name);
        if let Err(error) = self.docker.remove(&occupant.container).await
            && !is_not_found(&error)
        {
            // Not known to have stopped: its slot stays its until `collect_finished` sees that (ADR 0076).
            if is_being_removed(&error) {
                return;
            }
            warn!("cannot remove {}: {error}", occupant.runner_name);
            self.docker_failed(&error);
            return;
        }
        self.end_capture(&occupant.runner_name).await;
        // Its capture may have named its job as it ended.
        let exit = Exit::Killed {
            busy: self.occupant(slot).is_some_and(Occupant::engaged),
        };
        // Responses the mirror logged since this tick's count: on a shutdown, the mirror and its log go next.
        self.count_mirror_bytes().await;
        self.note_exit(&occupant.repository, &slot.profile, &exit);
        self.forget(&occupant);
        match self
            .github
            .delete_runner(&occupant.repository, occupant.runner_id)
            .await
        {
            // GitHub refuses until it notices the job is gone, minutes later, even if the run is cancelled.
            Ok(Deleted::Busy) => {
                info!(
                    "{}: GitHub keeps {} until it notices its job is gone; a later listing deletes it",
                    occupant.repository, occupant.runner_name
                );
                self.registrations
                    .ended_here(occupant.runner_id, &occupant.repository);
            }
            Ok(_) => {}
            Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
        }
        self.vacate(slot, &exit);
    }

    /// Removes the idle and warm runners a detaching stop leaves no work for (ADR 0091). An idle runner left
    /// registered would take one more job with no daemon to watch it. Adopted runners with no job named go too:
    /// adoption counts them busy until a listing says otherwise, and a stop may come before one. GitHub's `Busy` keeps
    /// those that do run a job (`remove_idle`).
    pub(super) async fn remove_every_idle(&mut self) {
        let idle: Vec<(SlotRef, StopReason)> = self
            .occupied()
            .filter(|(_, occupant)| occupant.goes_when_detaching())
            .map(|(slot, _)| (slot, StopReason::Shutdown))
            .collect();
        self.remove_idle(idle).await;
    }

    /// Removes the idle runners of `stops`, each for its reason, all at once (ADR 0091, 0092): each is deregistered,
    /// a few at a time as GitHub asks, then the containers are removed together, as each removal waits for its
    /// container to stop, and only then their log captures end, which they do with their containers. Waiting for a
    /// capture before its removal held every later start back by `CAPTURE_DRAIN`. GitHub's `Busy` keeps a runner that
    /// just took a job, as does one whose capture named a job by the time GitHub answered, or whose registration is
    /// already gone and whose logs show a job: its job may have ended meanwhile, and its logs and record come before
    /// its container goes. Busy runners stay, as does one whose
    /// deregistration, logs or removal fails: its slot and reservation stay its until a removal is confirmed
    /// (ADR 0076). Nothing once Docker stopped answering a stop.
    pub(super) async fn remove_idle(&mut self, stops: Vec<(SlotRef, StopReason)>) {
        if self.control.docker_gone() {
            return;
        }
        // A slot twice in `stops` is removed once.
        let stops: BTreeMap<SlotRef, StopReason> = stops.into_iter().collect();
        let idle: Vec<(SlotRef, Occupant, StopReason)> = stops
            .into_iter()
            .filter_map(|(slot, reason)| {
                let occupant = self.occupant(&slot)?.clone();
                Some((slot, occupant, reason))
            })
            .collect();
        if idle.is_empty() {
            return;
        }
        let github = &*self.github;
        let deleted: Vec<_> = futures_util::stream::iter(idle)
            .map(|(slot, occupant, reason)| async move {
                let answer = github
                    .delete_runner(&occupant.repository, occupant.runner_id)
                    .await;
                (slot, occupant, reason, answer)
            })
            .buffer_unordered(super::tick::CONCURRENT_REQUESTS)
            .collect()
            .await;
        // A capture may have named a job since the plan.
        self.take_found();
        let mut removing = Vec::new();
        let mut unregistered = Vec::new();
        for (slot, occupant, reason, answer) in deleted {
            let took_job = self
                .occupant(&slot)
                .is_some_and(|now| now.named_job().is_some());
            match answer {
                // Its capture named a job since the plan, which may have ended before GitHub dropped its
                // registration: it ends as busy runners do, its logs and record before its container goes.
                Ok(_) if took_job => self.keep_busy(&slot),
                Ok(Deleted::Deleted) => removing.push((slot, occupant, reason)),
                // GitHub also drops a runner's registration once its job ends: one that took a job since the plan
                // ends as runners do, its logs read and its failed job recorded before its container goes.
                Ok(Deleted::NotFound) => unregistered.push((slot, occupant, reason)),
                Ok(Deleted::Busy) => self.keep_busy(&slot),
                Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
            }
        }
        let docker = &*self.docker;
        let read = futures_util::future::join_all(unregistered.into_iter().map(
            |(slot, occupant, reason)| async move {
                let logs = docker.logs(&occupant.container).await;
                (slot, occupant, reason, logs)
            },
        ))
        .await;
        for (slot, occupant, reason, logs) in read {
            match logs {
                Ok(logs) if logs.contains(TOOK_JOB) => self.keep_busy(&slot),
                Ok(_) => removing.push((slot, occupant, reason)),
                // Someone removed it: `collect_finished` frees its slot once it stopped.
                Err(error) if is_log_gone(&error) => {}
                // It may have run a job: it stays until its logs say.
                Err(error) => warn!("cannot read the log of {}: {error}", occupant.runner_name),
            }
        }
        let docker = &*self.docker;
        let removed = futures_util::future::join_all(removing.into_iter().map(
            |(slot, occupant, reason)| async move {
                let removed = docker.remove(&occupant.container).await;
                (slot, occupant, reason, removed)
            },
        ))
        .await;
        let mut any = false;
        for (slot, occupant, reason, removed) in removed {
            match removed {
                // Not known to have stopped: `collect_finished` frees its slot once it did.
                Err(error) if is_being_removed(&error) => {}
                Err(error) if !is_not_found(&error) => {
                    warn!("cannot remove {}: {error}", occupant.runner_name);
                    self.docker_failed(&error);
                }
                _ => {
                    info!(
                        "{}: removed idle {}: {}",
                        occupant.repository,
                        occupant.runner_name,
                        reason.words()
                    );
                    self.end_capture(&occupant.runner_name).await;
                    // Its capture may have named its job as it ended.
                    let exit = Exit::RemovedIdle {
                        busy: self.occupant(&slot).is_some_and(Occupant::engaged),
                    };
                    self.store
                        .runner_stopped_for(&occupant.runner_name, reason.as_str());
                    self.note_exit(&occupant.repository, &slot.profile, &exit);
                    self.forget(&occupant);
                    self.vacate(&slot, &exit);
                    any = true;
                }
            }
        }
        // What the mirror logged for them, as their containers went.
        if any {
            self.count_mirror_bytes().await;
        }
    }

    /// Keeps a slot's runner that took a job after all: it ends as busy runners do. Until its capture names the job,
    /// it counts against one of its pool's listed jobs, which may be the one it took (`claims_a_listed_job`).
    fn keep_busy(&mut self, slot: &SlotRef) {
        if let Some(entry) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
            .and_then(Slot::occupant_mut)
        {
            entry.engagement.kept_busy(self.listing);
        }
    }

    /// Holds back the next runners of a repository's pool in `profile` after one failed, longer each time in a row,
    /// and announces the first hold of a streak as `runner.failed`: a pool that stays broken says so once, not every
    /// 10 minutes, until a runner takes a job. Its repository's other pools go on. Runners that fail while it is held started before the hold, so they do not lengthen it.
    pub(super) fn hold(
        &mut self,
        repository: &str,
        profile: &str,
        cause: HoldCause,
        error: String,
    ) {
        let Some(hold) = self
            .repositories
            .listed_mut(repository)
            .map(|state| state.health_mut(profile))
            .and_then(|health| health.hold(cause, SystemTime::now()))
        else {
            return;
        };
        warn!(
            "{repository}: no runner starts in {profile} for {}s",
            hold.wait.as_secs()
        );
        if hold.first {
            self.store.event(Event::RunnerFailed {
                failure: RunnerFailure {
                    repository: repository.to_owned(),
                    profile: profile.to_owned(),
                    error,
                    held_for: hold.wait.as_secs(),
                },
            });
        }
    }

    /// Notes that GitHub's rate limit is used up until `until`: no runner starts before.
    pub(super) fn github_limited(&mut self, until: SystemTime) {
        if self.github_limit().is_none() {
            warn!(
                "GitHub's rate limit is used up; no runner starts for {}s",
                until
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs()
            );
        }
        self.github_limited_until = self.github_limited_until.max(Some(until));
    }

    /// Until when GitHub's rate limit is used up, while it is.
    pub(super) fn github_limit(&self) -> Option<SystemTime> {
        self.github_limited_until
            .filter(|until| *until > SystemTime::now())
    }

    /// After a start in `profile` that failed and left nothing in Docker, closes its build cache, deletes its
    /// `runner`'s registration in the background, as a release does, and holds back its pool. A start of its batch
    /// folded meanwhile does not wait for GitHub; a stop waits for the delete (`finish_deletes`), and the next runners
    /// listing deletes a registration it left.
    pub(super) fn abandon(
        &mut self,
        profile: &str,
        repository: &str,
        runner: &Runner,
        failure: String,
    ) {
        self.close_build_cache(&runner.name);
        let (github, owner) = (self.github.clone(), repository.to_owned());
        let (id, name) = (runner.id, runner.name.clone());
        lock(&self.deletes).spawn(async move {
            if let Err(error) = github.delete_runner(&owner, id).await {
                warn!("{owner}: cannot delete {name}: {error}");
            }
        });
        self.hold(repository, profile, HoldCause::Start, failure);
    }

    /// Starts the registry mirror, when enabled and down, for a runner about to start.
    pub(super) async fn start_mirror(&mut self) {
        let mirror = &self.config.caches.mirror;
        if mirror.enabled && self.mirror.needs_start() {
            match cache::mirror_up(&*self.docker, &self.names, mirror, &self.config.host.slice)
                .await
            {
                Ok(()) => self.mirror.started(mirror.memory.0),
                // Runners pull from Docker Hub directly when the mirror does not answer.
                Err(error) => warn!("cannot start the mirror: {error}"),
            }
        }
    }

    /// Creates what a runner in `slot` mounts and says whether it is there. Not yet when the slot's last try failed
    /// and the time for the next has not come.
    pub(super) async fn caches_ready(&mut self, slot: &SlotRef, now: SystemTime) -> bool {
        if self
            .prepare_failures
            .get(slot)
            .is_some_and(|failed| failed.retry_at > now)
        {
            return false;
        }
        let number = slot.index + 1;
        let Err(error) = cache::prepare(&*self.docker, &self.names, &slot.profile, number).await
        else {
            self.prepare_failures.remove(slot);
            return true;
        };
        warn!(
            "cannot prepare the caches of {} slot {number}: {error}",
            slot.profile
        );
        let failed = self.prepare_failures.entry(slot.clone()).or_default();
        // One failure costs nothing, as it holds no pool back (ADR 0096); from the second on Docker is spared.
        failed.retry_at = now
            + match failed.failures {
                0 => Duration::ZERO,
                failures => backoff(failures),
            };
        failed.failures += 1;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::last_said;

    #[test]
    fn a_warning_reaches_the_reason_past_the_lines_a_runner_ends_on() {
        let tail = "Connected to GitHub\n\
            Listening for Jobs\n\
            The runner no longer exists on the server\n\
            [RUNNER 2026-11-07 16:42:43Z INFO Listener] Runner execution has finished with return code 0\n\
            Runner listener exit with 0 return code, stop the service, no retry needed.\n\
            Exiting runner...\n\n";
        assert_eq!(
            last_said(Some(tail)),
            "Listening for Jobs / The runner no longer exists on the server / \
             [RUNNER 2026-11-07 16:42:43Z INFO Listener] Runner execution has finished with return code 0"
        );
        assert_eq!(last_said(Some(&"é".repeat(400))).chars().count(), 300);
        assert_eq!(last_said(Some("\n")), "nothing");
        assert_eq!(last_said(None), "nothing");
    }
}
