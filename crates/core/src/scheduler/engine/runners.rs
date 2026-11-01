//! A slot's runner from start to removal: registers it with a JIT config and starts its container, stops idle or
//! forced ones, frees the slots of ended containers and holds back a repository whose runners end without taking
//! a job. It must not decide which slots start or stop: `tick` does, through `plan`.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use tracing::{info, warn};

use super::{DockerInStop, Engine, Occupant, Purpose, Shutdown, unix_secs};
use crate::api::{Admission, Event, HoldCause, RunnerFailure};
use crate::cache;
use crate::docker::{DockerApi, RunnerSpec, is_not_found};
use crate::github::{Deleted, GithubApi, Runner};
use crate::record;
use crate::scheduler::{Hold, IDLE_TIMEOUT, SlotRef, StopReason};

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

/// The labels of a runner container: what a restart adopts it by.
fn container_labels(
    name: &str,
    repository: &str,
    slot: usize,
    runner_id: u64,
    labels: &[String],
    image: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("runnerd.job".to_owned(), name.to_owned()),
        ("runnerd.repository".to_owned(), repository.to_owned()),
        ("runnerd.slot".to_owned(), slot.to_string()),
        ("runnerd.runner-id".to_owned(), runner_id.to_string()),
        (LABELS.to_owned(), labels.join(",")),
        (IMAGE.to_owned(), image.to_owned()),
        (SPEC.to_owned(), THIS_SPEC.to_owned()),
    ])
}

/// Where the next backoff of a repository whose runners keep failing ends.
pub(super) fn backoff(failures: u32) -> Duration {
    Duration::from_secs(30 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(600))
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Frees the slots whose container stopped: the runner took its job and exited, or it never started. First
    /// removes again the containers recovery failed to remove, and keeps or removes those it could not tell about
    /// once their backoff is over.
    pub(super) async fn collect_finished(&mut self) {
        let now = tokio::time::Instant::now();
        let (due, waiting) = std::mem::take(&mut self.unread)
            .into_iter()
            .partition(|left| left.due <= now);
        self.unread = waiting;
        for left in due {
            self.remove_left(left).await;
        }
        for (id, name) in std::mem::take(&mut self.leftover) {
            match self.docker.remove(&id).await {
                Err(error) if !is_not_found(&error) => {
                    warn!("cannot remove {name}: {error}");
                    self.leftover.insert(id, name);
                }
                _ => info!("removed {name}, left by an earlier run"),
            }
        }
        let occupied: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        for (slot, occupant) in occupied {
            let running = match self.docker.inspect(&occupant.container).await {
                // A kill in the last interval before it stopped, which no sample of its cgroup saw.
                Ok(state) if state.oom_killed => {
                    self.store.runner_oom_killed(&occupant.runner_name);
                    state.running
                }
                Ok(state) => state.running,
                Err(error) if is_not_found(&error) => false,
                // Its slot stays its until Docker says it stopped (ADR 0076); a stop leaves it to the next start.
                Err(error) => {
                    warn!("cannot inspect {}: {error}", occupant.runner_name);
                    self.docker_failed(&error);
                    continue;
                }
            };
            if !running {
                info!("{}: {} ended", occupant.repository, occupant.runner_name);
                self.release(&slot, &occupant).await;
            }
        }
    }

    /// Removes the container of a slot's runner that ended and its registration, then frees the slot. If its runner
    /// took no job, the repository's next runners are held back for a while. A container Docker fails to remove keeps
    /// its slot (ADR 0076): the next tick tries again, and a stop leaves it to the next start.
    async fn release(&mut self, slot: &SlotRef, occupant: &Occupant) {
        // The runner logs each job it takes and how it ended; its exit code says nothing (run.sh exits 0 when it
        // fails to start).
        let logs = match self.docker.logs(&occupant.container).await {
            Ok(logs) => Some(logs),
            Err(error) => {
                warn!("cannot read the log of {}: {error}", occupant.runner_name);
                None
            }
        };
        let took_job = logs.as_ref().map(|logs| logs.contains("Running job: "));
        // The capture ends with the container; its last lines must be stored before the job's record closes.
        self.end_capture(&occupant.runner_name).await;
        self.count_mirror_bytes().await;
        // A failed job's container is kept as its record, then removed, in the background.
        let recorded =
            logs.as_deref().is_some_and(record::failed) && self.begin_record(slot, occupant).await;
        if !recorded {
            match self.docker.remove(&occupant.container).await {
                Err(error) if !is_not_found(&error) => {
                    warn!("cannot remove {}: {error}", occupant.runner_name);
                    self.docker_failed(&error);
                    return;
                }
                _ => {}
            }
        }
        match self
            .github
            .delete_runner(&occupant.repository, occupant.runner_id)
            .await
        {
            // Busy: left to the next poll's cleanup of runners without a container.
            Ok(_) => {}
            Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
        }
        self.forget(occupant);
        // Unread logs: it may have run one.
        let ran = occupant.busy || occupant.job.is_some() || took_job != Some(false);
        if let Some(state) = self
            .repositories
            .get_mut(&occupant.repository)
            .map(|state| state.holds.entry(slot.profile.clone()).or_default())
        {
            match took_job {
                Some(true) => {
                    state.failures = 0;
                    state.idle_exits = 0;
                    state.held = None;
                }
                // One that waited `IDLE_TIMEOUT` had started and connected: a warm runner whose container went
                // once (a restart of Docker, the runner removed on GitHub) says nothing of the next one's start. The
                // second in a row is a fault that would restart one every few minutes.
                Some(false)
                    if occupant
                        .started_at
                        .elapsed()
                        .is_ok_and(|waited| waited >= IDLE_TIMEOUT) =>
                {
                    state.idle_exits += 1;
                    // It started and connected: a failure before it is over, and the next one is a new streak's
                    // first, announced. Not so for the second in a row, a fault of its own.
                    if state.idle_exits == 1 {
                        state.failures = 0;
                    }
                    warn!(
                        "{}: {} ended without a job after waiting for one",
                        occupant.repository, occupant.runner_name
                    );
                    if state.idle_exits >= 2 {
                        self.hold(
                            &occupant.repository,
                            &slot.profile,
                            HoldCause::IdleExits,
                            "its runners end without a job after waiting for one".to_owned(),
                        );
                    }
                }
                Some(false) => {
                    warn!(
                        "{}: {} ended without taking a job",
                        occupant.repository, occupant.runner_name
                    );
                    self.hold(
                        &occupant.repository,
                        &slot.profile,
                        HoldCause::NoJob,
                        format!("{} ended without taking a job", occupant.runner_name),
                    );
                }
                None => {}
            }
        }
        self.vacate(slot, ran);
    }

    /// Frees a slot whose runner is gone. Its runner may have grown the shared caches, and if it `ran` a job in slot
    /// 1, its profile's layers: both are checked once idle. A killed job's partial writes count too: they stay in
    /// slot 1 and would reach the snapshot on any later reseed, like those of a job cancelled or out of memory. A
    /// warm runner that leaves slot 1 without a job changed nothing there.
    fn vacate(&mut self, slot: &SlotRef, ran: bool) {
        let left = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
            .and_then(Option::take);
        // Its layer holds what that job cached. A runner that ran a job nobody named leaves the layer to a job
        // unknown, and one that ran none leaves it as it was.
        match left.and_then(|occupant| occupant.key) {
            Some(key) => {
                self.ran.insert(slot.clone(), key);
            }
            None if ran => {
                self.ran.remove(slot);
            }
            None => {}
        }
        if slot.index == 0 && ran {
            self.reseed_due.insert(slot.profile.clone());
        }
        self.trim_due = true;
        self.drop_closed_slots();
    }

    /// A runner container's environment: its slot's share of what the slots' Docker Engines may keep, which its
    /// image's job cleanup holds it to, once the caches' check has measured Docker's disk.
    fn runner_env(&self) -> Vec<String> {
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
        if self.docker_in_stop == DockerInStop::Failed {
            return;
        }
        let Some(occupant) = self
            .slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .cloned()
            .flatten()
        else {
            return;
        };
        let busy = occupant.busy || occupant.job.is_some();
        let what = if busy { "killing" } else { "removing idle" };
        info!("{}: {what} {}", occupant.repository, occupant.runner_name);
        if let Err(error) = self.docker.remove(&occupant.container).await
            && !is_not_found(&error)
        {
            warn!("cannot remove {}: {error}", occupant.runner_name);
            self.docker_failed(&error);
            return;
        }
        self.end_capture(&occupant.runner_name).await;
        // Responses the mirror logged since this tick's count: on a shutdown, the mirror and its log go next.
        self.count_mirror_bytes().await;
        self.forget(&occupant);
        match self
            .github
            .delete_runner(&occupant.repository, occupant.runner_id)
            .await
        {
            // GitHub refuses until it notices the job is gone, minutes later, even if the run is cancelled.
            Ok(Deleted::Busy) => warn!(
                "{}: GitHub keeps {} until it notices its job is gone; the next runnerd start deletes it",
                occupant.repository, occupant.runner_name
            ),
            Ok(_) => {}
            Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
        }
        self.vacate(slot, busy);
    }

    /// Removes the idle and warm runners a detaching stop leaves no work for (ADR 0091). An idle runner left
    /// registered would take one more job with no daemon to watch it.
    pub(super) async fn remove_every_idle(&mut self) {
        let idle: Vec<(SlotRef, StopReason)> = self
            .occupied()
            .filter(|(_, occupant)| !occupant.busy && occupant.job.is_none())
            .map(|(slot, _)| (slot, StopReason::Shutdown))
            .collect();
        self.remove_idle(idle).await;
    }

    /// Removes the idle runners of `stops`, each for its reason, all at once (ADR 0091, 0092): each is deregistered,
    /// a few at a time as GitHub asks, then the containers are removed together, as each removal waits for its
    /// container to stop, and only then their log captures end, which they do with their containers. Waiting for a
    /// capture before its removal held every later start back by `CAPTURE_DRAIN`. GitHub's `Busy` keeps a runner that
    /// just took a job, as does a registration already gone whose capture or logs show a job: its job may have ended
    /// meanwhile, and its logs and record come before its container goes. Busy runners stay, as does one whose
    /// deregistration, logs or removal fails: its slot and reservation stay its until a removal is confirmed
    /// (ADR 0076). Nothing once Docker stopped answering a stop.
    pub(super) async fn remove_idle(&mut self, stops: Vec<(SlotRef, StopReason)>) {
        if self.docker_in_stop == DockerInStop::Failed {
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
            match answer {
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
            let took_job = self.occupant(&slot).is_some_and(|now| now.job.is_some());
            match logs {
                _ if took_job => self.keep_busy(&slot),
                Ok(logs) if logs.contains("Running job: ") => self.keep_busy(&slot),
                Ok(_) => removing.push((slot, occupant, reason)),
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
                    self.store
                        .runner_stopped_for(&occupant.runner_name, reason.as_str());
                    self.forget(&occupant);
                    self.vacate(&slot, occupant.busy || occupant.job.is_some());
                    any = true;
                }
            }
        }
        // What the mirror logged for them, as their containers went.
        if any {
            self.count_mirror_bytes().await;
        }
    }

    /// Keeps a slot's runner that took a job after all: it ends as busy runners do.
    fn keep_busy(&mut self, slot: &SlotRef) {
        if let Some(Some(entry)) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
        {
            entry.busy = true;
        }
    }

    /// Holds back the next runners of a repository's pool in `profile` after one failed, longer each time in a row,
    /// and announces the first hold of a streak as `runner.failed`: a pool that stays broken says so once, not every
    /// 10 minutes, until a runner takes a job. Its repository's other pools go on. Runners that fail while it is held started before the hold, so they do not lengthen it.
    fn hold(&mut self, repository: &str, profile: &str, cause: HoldCause, error: String) {
        if let Some(state) = self
            .repositories
            .get_mut(repository)
            .map(|state| state.holds.entry(profile.to_owned()).or_default())
            && state
                .held
                .is_none_or(|held| held.until <= SystemTime::now())
        {
            state.failures += 1;
            let wait = backoff(state.failures);
            state.held = Some(Hold {
                until: SystemTime::now() + wait,
                cause,
            });
            warn!(
                "{repository}: no runner starts in {profile} for {}s",
                wait.as_secs()
            );
            if state.failures > 1 {
                return;
            }
            self.store.event(Event::RunnerFailed {
                failure: RunnerFailure {
                    repository: repository.to_owned(),
                    profile: profile.to_owned(),
                    error,
                    held_for: wait.as_secs(),
                },
            });
        }
    }

    /// After a start in `profile` that failed and left nothing in Docker, deletes its `runner`'s registration and
    /// closes its build cache, and holds back its pool.
    async fn abandon(&mut self, profile: &str, repository: &str, runner: &Runner, failure: String) {
        self.close_build_cache(&runner.name);
        if let Err(error) = self.github.delete_runner(repository, runner.id).await {
            warn!("{repository}: cannot delete {}: {error}", runner.name);
        }
        self.hold(repository, profile, HoldCause::Start, failure);
    }

    /// Starts the registry mirror, when enabled and down, for a runner about to start.
    async fn start_mirror(&mut self) {
        let mirror = &self.config.caches.mirror;
        if mirror.enabled && !self.mirror_up {
            match cache::mirror_up(&*self.docker, &self.names, mirror, &self.config.host.slice)
                .await
            {
                Ok(()) => {
                    self.mirror_up = true;
                    self.mirror_memory = mirror.memory.0;
                }
                // Runners pull from Docker Hub directly when the mirror does not answer.
                Err(error) => warn!("cannot start the mirror: {error}"),
            }
        }
    }

    pub(super) async fn start(
        &mut self,
        slot: &SlotRef,
        repository: &str,
        admission: Admission,
        purpose: Purpose,
    ) {
        // Its pool's labels: those of the repository's entry in the slot's profile.
        let Some(labels) = self
            .config
            .pool_in(repository, &slot.profile)
            .map(|pool| pool.labels.clone())
        else {
            return;
        };
        let number = slot.index + 1;
        let now = SystemTime::now();
        let name = self
            .identity
            .runner_name(&slot.profile, number, unix_secs(now));
        if let Err(error) = cache::prepare(&*self.docker, &self.names, &slot.profile, number).await
        {
            warn!(
                "cannot prepare the caches of {} slot {number}: {error}",
                slot.profile
            );
            return;
        }
        self.start_mirror().await;
        let Some(profile) = self.config.profiles.get(&slot.profile) else {
            return;
        };
        let (runner, jit_config) = match self.github.jit_config(repository, &name, &labels).await {
            Ok(registered) => registered,
            Err(error) => {
                let failure = format!("cannot register a runner: {error}");
                warn!("{repository}: {failure}");
                self.hold(repository, &slot.profile, HoldCause::Register, failure);
                return;
            }
        };
        // A stop asked for while GitHub registered it: its runner would take a job no daemon watches.
        if self.stopping() != Shutdown::No {
            info!("{repository}: stopping; deleting {name} before it starts");
            if let Err(error) = self.github.delete_runner(repository, runner.id).await {
                warn!("{repository}: cannot delete {name}: {error}");
            }
            return;
        }
        let image = profile.image.reference();
        let spec = RunnerSpec {
            name: name.clone(),
            image: image.clone(),
            profile: slot.profile.clone(),
            slice: self.config.host.slice.clone(),
            cpus: profile.cpus,
            memory: profile.memory,
            env: self.runner_env(),
            mounts: self.runner_mounts(&slot.profile, number, &name, repository),
            network: Some(self.names.network()),
            labels: container_labels(&name, repository, number, runner.id, &labels, &image),
            entrypoint: None,
            jit_config: Some(jit_config),
        };
        let (container, failure) = match self.docker.start(&spec, self.isolation).await {
            Ok(container) => (container, None),
            Err(failed) => {
                let failure = format!("cannot start {name}: {}", failed.error);
                warn!("{repository}: {failure}");
                self.recheck_image(&spec.image);
                let Some(container) = failed.container else {
                    return self
                        .abandon(&slot.profile, repository, &runner, failure)
                        .await;
                };
                (container, Some(failure))
            }
        };
        let occupant = Occupant {
            repository: repository.to_owned(),
            runner_name: name.clone(),
            runner_id: runner.id,
            container,
            busy: false,
            started_at: now,
            memory: spec.memory.0,
            cpus: u64::from(spec.cpus) * 1000,
            admission: Some(admission),
            job: None,
            key: None,
            labels: Some(labels),
            image: Some(image),
            spec: Some(THIS_SPEC.to_owned()),
            observed: false,
            purpose: Some(purpose),
        };
        self.place(slot, occupant, purpose, failure).await;
    }

    /// Puts a runner a start made in its slot and watches it, or after a start that failed, its container that
    /// Docker kept: it holds the slot, with its registration, until `collect_finished` confirms its removal, which
    /// keeps a second runner off the slot's volumes. If it runs, the next tick watches it as a runner.
    async fn place(
        &mut self,
        slot: &SlotRef,
        occupant: Occupant,
        purpose: Purpose,
        failure: Option<String>,
    ) {
        if let Some(entry) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
        {
            *entry = Some(occupant.clone());
        }
        let (repository, name, number) =
            (&occupant.repository, &occupant.runner_name, slot.index + 1);
        match failure {
            None => {
                let warm = match purpose {
                    Purpose::Job => "",
                    Purpose::Warm => " warm, for jobs not queued yet",
                    Purpose::Burst => " warm, for a job its runs are expected to queue",
                };
                info!(
                    "{repository}: started {name} in {} slot {number}{warm}",
                    slot.profile
                );
                self.observe(slot, &occupant).await;
            }
            Some(failure) => {
                warn!(
                    "{repository}: {name} may be left in Docker; {} slot {number} stays its until it is removed",
                    slot.profile
                );
                self.hold(repository, &slot.profile, HoldCause::Start, failure);
            }
        }
    }
}
