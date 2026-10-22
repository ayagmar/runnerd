//! The scheduling loop: polls GitHub, applies `plan`'s decisions with GitHub and Docker, removes each runner's
//! container when its job ends, keeps the mirror up while any runner runs, reseeds idle profiles and recovers
//! runners and containers a previous daemon left. It owns slot state; it never serves the API (it publishes
//! `api::Status` and takes commands through `Control`) and never logs the token or a JIT config.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{Decision, Profile, Repository, RunnerSlot, Slot, SlotRef, plan};
use crate::api;
use crate::cache::{self, Names, runner_mounts};
use crate::config::{ByteSize, Config};
use crate::docker::{self, Docker, RunnerSpec, is_not_found};
use crate::github::{self, Deleted, Github, QueuedJob};
use crate::isolation::{self, Isolation};

/// The image a profile's runners run.
pub fn image(stage: &str) -> String {
    format!("runnerd-runner:{stage}")
}

/// This machine's name as runner names carry it: lower case, letters, digits and dashes, at most 20 characters.
pub fn host_name() -> String {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let name: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(20)
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "host".to_owned()
    } else {
        name.to_owned()
    }
}

/// How the daemon is asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shutdown {
    /// Keep scheduling.
    No,
    /// Start nothing new, let running jobs finish.
    Graceful,
    /// Kill running jobs and delete their runners.
    Force,
}

/// Commands from the API and signals to the loop.
#[derive(Debug)]
pub struct Control {
    repositories: BTreeSet<String>,
    paused: Mutex<BTreeSet<String>>,
    shutdown: Mutex<Shutdown>,
    wake: Notify,
}

impl Control {
    /// Controls for the configured repositories.
    pub fn new(repositories: impl IntoIterator<Item = String>) -> Control {
        Control {
            repositories: repositories.into_iter().collect(),
            paused: Mutex::new(BTreeSet::new()),
            shutdown: Mutex::new(Shutdown::No),
            wake: Notify::new(),
        }
    }

    /// Whether a shutdown was requested, and how.
    pub fn mode(&self) -> Shutdown {
        *lock(&self.shutdown)
    }

    /// Pauses or resumes a repository; `false` if it is not configured.
    pub fn set_paused(&self, repository: &str, paused: bool) -> bool {
        if !self.repositories.contains(repository) {
            return false;
        }
        let mut set = lock(&self.paused);
        if paused {
            set.insert(repository.to_owned());
        } else {
            set.remove(repository);
        }
        drop(set);
        self.wake.notify_one();
        true
    }

    /// Asks the loop to stop; a forced request overrides a graceful one, never the other way.
    pub fn shutdown(&self, force: bool) {
        let mode = if force {
            Shutdown::Force
        } else {
            Shutdown::Graceful
        };
        let mut shutdown = lock(&self.shutdown);
        *shutdown = (*shutdown).max(mode);
        drop(shutdown);
        self.wake.notify_one();
    }

    /// Whether the repository is paused.
    pub fn is_paused(&self, repository: &str) -> bool {
        lock(&self.paused).contains(repository)
    }
}

/// What the API needs from a running engine.
#[derive(Debug, Clone)]
pub struct Handle {
    pub status: watch::Receiver<api::Status>,
    pub control: Arc<Control>,
}

/// A slot's runner and its container.
#[derive(Debug, Clone)]
struct Occupant {
    repository: String,
    runner_name: String,
    runner_id: u64,
    container: String,
    busy: bool,
    started_at: SystemTime,
}

#[derive(Debug)]
struct RepositoryState {
    queued: Vec<QueuedJob>,
    held_until: Option<SystemTime>,
    /// Runners in a row that ended without taking a job.
    failures: u32,
}

/// The shared caches `trim_shared` emptied, with their sizes.
type Emptied = Vec<(String, ByteSize)>;

/// The scheduler with its side effects.
pub struct Engine {
    config: Config,
    github: Github,
    docker: Arc<Docker>,
    names: Names,
    isolation: &'static dyn Isolation,
    runner_prefix: String,
    memory_max: Option<u64>,
    slots: BTreeMap<String, Vec<Option<Occupant>>>,
    repositories: BTreeMap<String, RepositoryState>,
    reseeds: BTreeMap<String, JoinHandle<Result<String, docker::Error>>>,
    /// Profiles whose slot 1 ran a job since their last reseed check.
    reseed_due: BTreeSet<String>,
    /// Whether a job ended since the shared caches were last checked.
    trim_due: bool,
    /// The running check of the shared caches: Docker takes many seconds to measure volumes.
    trim: Option<JoinHandle<Result<Emptied, docker::Error>>>,
    mirror_up: bool,
    rate_limit_logged: bool,
    /// Runners without a container that GitHub still holds busy, already logged.
    busy_orphans: BTreeSet<u64>,
    status: watch::Sender<api::Status>,
    control: Arc<Control>,
}

/// Where the next backoff of a repository whose runners keep failing ends.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(30 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(600))
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // The guarded data stays consistent: every critical section is a single insert, remove or read.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Engine {
    /// An engine for a validated config. `host` names this machine's runners (`host_name`), so a restarted daemon
    /// recognizes them; `memory_max` is the slice's.
    pub fn new(
        config: Config,
        github: Github,
        docker: Docker,
        names: Names,
        host: &str,
        memory_max: Option<u64>,
    ) -> (Engine, Handle) {
        let slots = config
            .profiles
            .iter()
            .map(|(name, profile)| (name.clone(), vec![None; profile.slots as usize]))
            .collect();
        let repositories = config
            .repositories
            .iter()
            .map(|repository| {
                (
                    repository.name.clone(),
                    RepositoryState {
                        queued: Vec::new(),
                        held_until: None,
                        failures: 0,
                    },
                )
            })
            .collect();
        let control = Arc::new(Control::new(
            config
                .repositories
                .iter()
                .map(|repository| repository.name.clone()),
        ));
        let isolation = isolation::backend(config.host.isolation);
        let (status, receiver) = watch::channel(api::Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            shutting_down: false,
            budget: api::Budget {
                slice: config.host.slice.clone(),
                memory_max,
                memory_in_use: 0,
            },
            profiles: Vec::new(),
            repositories: Vec::new(),
        });
        let mut engine = Engine {
            config,
            github,
            docker: Arc::new(docker),
            names,
            isolation,
            runner_prefix: format!("runnerd-{host}-"),
            memory_max,
            slots,
            repositories,
            reseeds: BTreeMap::new(),
            reseed_due: BTreeSet::new(),
            trim_due: false,
            trim: None,
            // Unknown: an earlier run may have left it up, so the first idle tick stops it.
            mirror_up: true,
            rate_limit_logged: false,
            busy_orphans: BTreeSet::new(),
            status,
            control: control.clone(),
        };
        engine.publish();
        (
            engine,
            Handle {
                status: receiver,
                control,
            },
        )
    }

    /// Runs until a shutdown request is carried out.
    pub async fn run(mut self) {
        self.recover().await;
        loop {
            let mode = self.control.mode();
            self.tick(mode).await;
            self.publish();
            let idle = self.slots.values().flatten().all(Option::is_none);
            if mode != Shutdown::No && idle && self.reseeds.is_empty() && self.trim.is_none() {
                break;
            }
            let interval = if mode == Shutdown::No {
                self.config.host.poll_interval
            } else {
                Duration::from_secs(5)
            };
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = self.control.wake.notified() => {}
            }
        }
        if let Err(error) = cache::mirror_down(&self.docker, &self.names).await {
            warn!("cannot stop the mirror: {error}");
        }
        info!("stopped");
    }

    /// Adopts runner containers a previous daemon left running, and removes the ones that stopped. Their
    /// registrations, and any of this host's runners without a container, go at the first poll.
    async fn recover(&mut self) {
        let containers = match self.docker.list("runnerd.job").await {
            Ok(containers) => containers,
            Err(error) => {
                warn!("cannot list runner containers left by an earlier run: {error}");
                return;
            }
        };
        for container in containers {
            if !container.name.starts_with(&self.runner_prefix) {
                continue;
            }
            let label = |name: &str| container.labels.get(name).cloned().unwrap_or_default();
            let profile = label("runnerd.profile");
            let index = label("runnerd.slot")
                .parse::<usize>()
                .ok()
                .and_then(|slot| slot.checked_sub(1));
            let occupant = Occupant {
                repository: label("runnerd.repository"),
                runner_name: container.name.clone(),
                runner_id: label("runnerd.runner-id").parse().unwrap_or(0),
                container: container.id.clone(),
                busy: true,
                started_at: SystemTime::now(),
            };
            let known_repository = self.repositories.contains_key(&occupant.repository);
            let free = index.and_then(|index| {
                self.slots
                    .get_mut(&profile)?
                    .get_mut(index)
                    .filter(|slot| slot.is_none())
            });
            match free {
                Some(slot) if container.running && known_repository => {
                    info!(
                        "adopted {} in {profile} slot {}",
                        occupant.runner_name,
                        index.map_or(0, |index| index + 1)
                    );
                    *slot = Some(occupant);
                }
                _ => {
                    info!("removing {}, left by an earlier run", container.name);
                    if let Err(error) = self.docker.remove(&container.id).await {
                        warn!("cannot remove {}: {error}", container.name);
                    }
                }
            }
        }
    }

    async fn tick(&mut self, mode: Shutdown) {
        self.collect_reseeds().await;
        self.collect_trim().await;
        self.collect_finished().await;
        // Runners first: a runner that takes its job after this poll still counts as idle against a job the later
        // queued-jobs poll no longer lists, which leaves it surplus at worst (it stops after IDLE_TIMEOUT). The other
        // order sees the job queued and its runner busy, and starts a second runner for it.
        self.poll_runners().await;
        if mode == Shutdown::No {
            self.poll_queued().await;
        }
        let decisions = match mode {
            Shutdown::No => self.plan(),
            Shutdown::Graceful | Shutdown::Force => self
                .occupied()
                .filter(|(_, occupant)| mode == Shutdown::Force || !occupant.busy)
                .map(|(slot, _)| Decision::Stop { slot })
                .collect(),
        };
        for decision in decisions {
            match decision {
                Decision::Stop { slot } => self.stop(&slot, mode == Shutdown::Force).await,
                Decision::Start { slot, repository } => self.start(&slot, &repository).await,
            }
        }
        self.idle_caches(mode).await;
    }

    fn occupied(&self) -> impl Iterator<Item = (SlotRef, &Occupant)> {
        self.slots.iter().flat_map(|(profile, slots)| {
            slots.iter().enumerate().filter_map(|(index, slot)| {
                slot.as_ref().map(|occupant| {
                    (
                        SlotRef {
                            profile: profile.clone(),
                            index,
                        },
                        occupant,
                    )
                })
            })
        })
    }

    fn plan(&self) -> Vec<Decision> {
        let repositories: Vec<Repository> = self
            .config
            .repositories
            .iter()
            .map(|repository| {
                let state = &self.repositories[&repository.name];
                Repository {
                    name: repository.name.clone(),
                    profile: repository.profile.clone(),
                    labels: repository.labels.clone(),
                    paused: self.control.is_paused(&repository.name),
                    held_until: state.held_until,
                    queued: state.queued.clone(),
                }
            })
            .collect();
        let profiles = self
            .config
            .profiles
            .iter()
            .map(|(name, profile)| {
                let slots = self.slots[name]
                    .iter()
                    .map(|slot| match slot {
                        None => Slot::Free,
                        Some(occupant) => Slot::Runner(RunnerSlot {
                            repository: occupant.repository.clone(),
                            busy: occupant.busy,
                            started_at: occupant.started_at,
                        }),
                    })
                    .collect();
                let profile = Profile {
                    memory: profile.memory.0,
                    reseeding: self.reseeds.contains_key(name),
                    slots,
                };
                (name.clone(), profile)
            })
            .collect();
        let free_memory = self
            .memory_max
            .map(|max| max.saturating_sub(self.memory_in_use()));
        plan(&repositories, &profiles, free_memory, SystemTime::now())
    }

    fn memory_in_use(&self) -> u64 {
        let mirror = &self.config.caches.mirror;
        let mirror = if mirror.enabled { mirror.memory.0 } else { 0 };
        self.occupied()
            .map(|(slot, _)| self.config.profiles[&slot.profile].memory.0)
            .sum::<u64>()
            + mirror
    }

    async fn poll_queued(&mut self) {
        for repository in &self.config.repositories {
            let queued = match self.github.queued_jobs(&repository.name).await {
                Ok(queued) => {
                    self.rate_limit_logged = false;
                    queued
                }
                Err(github::Error::RateLimited { until }) => {
                    if !self.rate_limit_logged {
                        warn!(
                            "GitHub's rate limit is used up; polling again in {}s",
                            until
                                .duration_since(SystemTime::now())
                                .unwrap_or_default()
                                .as_secs()
                        );
                        self.rate_limit_logged = true;
                    }
                    Vec::new()
                }
                Err(error) => {
                    warn!("{}: cannot list queued jobs: {error}", repository.name);
                    Vec::new()
                }
            };
            if let Some(state) = self.repositories.get_mut(&repository.name) {
                state.queued = queued;
            }
        }
    }

    /// Updates each runner's busy flag and deletes this host's runners that no slot holds.
    async fn poll_runners(&mut self) {
        let names: Vec<String> = self.repositories.keys().cloned().collect();
        for repository in names {
            let runners = match self.github.runners(&repository).await {
                Ok(runners) => runners,
                Err(github::Error::RateLimited { .. }) => continue,
                Err(error) => {
                    warn!("{repository}: cannot list runners: {error}");
                    continue;
                }
            };
            for runner in runners {
                if !runner.name.starts_with(&self.runner_prefix) {
                    continue;
                }
                let held = self
                    .slots
                    .values_mut()
                    .flatten()
                    .flatten()
                    .find(|occupant| occupant.runner_id == runner.id);
                match held {
                    Some(occupant) => occupant.busy = runner.busy,
                    None => match self.github.delete_runner(&repository, runner.id).await {
                        Ok(Deleted::Deleted) => {
                            info!(
                                "{repository}: deleted {}, which had no container",
                                runner.name
                            );
                        }
                        Ok(Deleted::NotFound) => {}
                        Ok(Deleted::Busy) => {
                            if self.busy_orphans.insert(runner.id) {
                                warn!(
                                    "{repository}: {} has no container, but GitHub says it runs a job; \
                                     deleting it once GitHub lets go",
                                    runner.name
                                );
                            }
                        }
                        Err(error) => warn!("{repository}: cannot delete {}: {error}", runner.name),
                    },
                }
            }
        }
    }

    /// Frees the slots whose container stopped: the runner took its job and exited, or it never started.
    async fn collect_finished(&mut self) {
        let occupied: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        for (slot, occupant) in occupied {
            let running = match self.docker.inspect(&occupant.container).await {
                Ok(state) => state.running,
                Err(error) if is_not_found(&error) => false,
                Err(error) => {
                    warn!("cannot inspect {}: {error}", occupant.runner_name);
                    continue;
                }
            };
            if !running {
                info!("{}: {} ended", occupant.repository, occupant.runner_name);
                self.release(&slot, &occupant, true).await;
            }
        }
    }

    /// Removes a slot's container and its runner, then frees the slot. With `ended`, the container stopped on its
    /// own, and if its runner took no job, the repository's next runners are held back for a while.
    async fn release(&mut self, slot: &SlotRef, occupant: &Occupant, ended: bool) {
        // The runner logs each job it takes; its exit code says nothing (run.sh exits 0 when it fails to start).
        let took_job = if ended {
            match self.docker.logs(&occupant.container).await {
                Ok(logs) => Some(logs.contains("Running job: ")),
                Err(error) => {
                    warn!("cannot read the log of {}: {error}", occupant.runner_name);
                    None
                }
            }
        } else {
            None
        };
        match self.docker.remove(&occupant.container).await {
            Err(error) if !is_not_found(&error) => {
                warn!("cannot remove {}: {error}", occupant.runner_name);
                return;
            }
            _ => {}
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
        if let Some(state) = self.repositories.get_mut(&occupant.repository) {
            match took_job {
                Some(true) => {
                    state.failures = 0;
                    state.held_until = None;
                }
                Some(false) => {
                    warn!(
                        "{}: {} ended without taking a job",
                        occupant.repository, occupant.runner_name
                    );
                    self.hold(&occupant.repository);
                }
                None => {}
            }
        }
        if let Some(entry) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
        {
            *entry = None;
        }
        if slot.index == 0 {
            self.reseed_due.insert(slot.profile.clone());
        }
        self.trim_due = true;
    }

    /// Removes an idle runner, or with `force` a busy one and its job.
    async fn stop(&mut self, slot: &SlotRef, force: bool) {
        let Some(occupant) = self
            .slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .cloned()
            .flatten()
        else {
            return;
        };
        if force {
            info!("{}: killing {}", occupant.repository, occupant.runner_name);
            if let Err(error) = self.docker.remove(&occupant.container).await
                && !is_not_found(&error)
            {
                warn!("cannot remove {}: {error}", occupant.runner_name);
                return;
            }
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
            if let Some(entry) = self
                .slots
                .get_mut(&slot.profile)
                .and_then(|slots| slots.get_mut(slot.index))
            {
                *entry = None;
            }
            return;
        }
        match self
            .github
            .delete_runner(&occupant.repository, occupant.runner_id)
            .await
        {
            Ok(Deleted::Busy) => {
                if let Some(Some(entry)) = self
                    .slots
                    .get_mut(&slot.profile)
                    .and_then(|slots| slots.get_mut(slot.index))
                {
                    entry.busy = true;
                }
            }
            Ok(Deleted::Deleted | Deleted::NotFound) => {
                info!(
                    "{}: removed idle {}",
                    occupant.repository, occupant.runner_name
                );
                self.release(slot, &occupant, false).await;
            }
            Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
        }
    }

    /// Holds back a repository's next runners after one failed, longer each time in a row.
    fn hold(&mut self, repository: &str) {
        if let Some(state) = self.repositories.get_mut(repository) {
            state.failures += 1;
            let wait = backoff(state.failures);
            state.held_until = Some(SystemTime::now() + wait);
            warn!("{repository}: no runner starts for {}s", wait.as_secs());
        }
    }

    async fn start(&mut self, slot: &SlotRef, repository: &str) {
        let Some(profile) = self.config.profiles.get(&slot.profile) else {
            return;
        };
        let Some(labels) = self
            .config
            .repositories
            .iter()
            .find(|candidate| candidate.name == repository)
            .map(|candidate| candidate.labels.clone())
        else {
            return;
        };
        let number = slot.index + 1;
        let now = SystemTime::now();
        let name = format!(
            "{}{}-{number}-{}",
            self.runner_prefix,
            slot.profile,
            unix_secs(now)
        );
        if let Err(error) = cache::prepare(&self.docker, &self.names, &slot.profile, number).await {
            warn!(
                "cannot prepare the caches of {} slot {number}: {error}",
                slot.profile
            );
            return;
        }
        let mirror = &self.config.caches.mirror;
        if mirror.enabled && !self.mirror_up {
            match cache::mirror_up(&self.docker, &self.names, mirror, &self.config.host.slice).await
            {
                Ok(()) => self.mirror_up = true,
                // Runners pull from Docker Hub directly when the mirror does not answer.
                Err(error) => warn!("cannot start the mirror: {error}"),
            }
        }
        let (runner, jit_config) = match self.github.jit_config(repository, &name, &labels).await {
            Ok(registered) => registered,
            Err(error) => {
                warn!("{repository}: cannot register a runner: {error}");
                self.hold(repository);
                return;
            }
        };
        let spec = RunnerSpec {
            name: name.clone(),
            image: image(&profile.image_stage),
            profile: slot.profile.clone(),
            slice: self.config.host.slice.clone(),
            cpus: profile.cpus,
            memory: profile.memory,
            env: Vec::new(),
            mounts: runner_mounts(&self.names, &slot.profile, number),
            network: Some(self.names.network()),
            labels: BTreeMap::from([
                ("runnerd.job".to_owned(), name.clone()),
                ("runnerd.repository".to_owned(), repository.to_owned()),
                ("runnerd.slot".to_owned(), number.to_string()),
                ("runnerd.runner-id".to_owned(), runner.id.to_string()),
            ]),
            entrypoint: None,
            jit_config: Some(jit_config),
        };
        match self.docker.start(&spec, self.isolation).await {
            Ok(container) => {
                info!(
                    "{repository}: started {name} in {} slot {number}",
                    slot.profile
                );
                if let Some(entry) = self
                    .slots
                    .get_mut(&slot.profile)
                    .and_then(|slots| slots.get_mut(slot.index))
                {
                    *entry = Some(Occupant {
                        repository: repository.to_owned(),
                        runner_name: name,
                        runner_id: runner.id,
                        container,
                        busy: false,
                        started_at: now,
                    });
                }
            }
            Err(error) => {
                warn!("{repository}: cannot start {name}: {error}");
                if let Err(error) = self.github.delete_runner(repository, runner.id).await {
                    warn!("{repository}: cannot delete {name}: {error}");
                }
                self.hold(repository);
            }
        }
    }

    /// Reseeds the idle profiles whose slot 1 ran a job; with no runner running at all, stops the mirror and
    /// empties the shared caches over their limit.
    async fn idle_caches(&mut self, mode: Shutdown) {
        if mode == Shutdown::No {
            let due: Vec<String> = self
                .reseed_due
                .iter()
                .filter(|name| {
                    self.slots
                        .get(*name)
                        .is_some_and(|slots| slots.iter().all(Option::is_none))
                })
                .cloned()
                .collect();
            for name in due {
                self.reseed_due.remove(&name);
                self.spawn_reseed(name);
            }
        }
        if self.occupied().next().is_some() {
            return;
        }
        if self.mirror_up {
            match cache::mirror_down(&self.docker, &self.names).await {
                Ok(()) => self.mirror_up = false,
                Err(error) => warn!("cannot stop the mirror: {error}"),
            }
        }
        if self.trim_due && self.trim.is_none() {
            self.trim_due = false;
            let docker = self.docker.clone();
            let names = self.names.clone();
            let caches = self.config.caches.clone();
            self.trim = Some(tokio::spawn(async move {
                cache::trim_shared(&docker, &names, &caches).await
            }));
        }
    }

    /// Rebuilds a profile's snapshot in the background; its slots start nothing until it is done.
    fn spawn_reseed(&mut self, name: String) {
        let Some(profile) = self.config.profiles.get(&name) else {
            return;
        };
        let docker = self.docker.clone();
        let names = self.names.clone();
        let image = image(&profile.image_stage);
        let slice = self.config.host.slice.clone();
        let (slots, cpus, memory) = (profile.slots as usize, profile.cpus, profile.memory);
        let min_changes = self.config.caches.reseed_at;
        let isolation = self.isolation;
        let profile_name = name.clone();
        let task = tokio::spawn(async move {
            cache::reseed(
                &docker,
                &cache::Reseed {
                    names: &names,
                    profile: &profile_name,
                    image: &image,
                    slots,
                    slice: &slice,
                    cpus,
                    memory,
                    min_changes,
                    isolation,
                },
            )
            .await
        });
        self.reseeds.insert(name, task);
    }

    async fn collect_trim(&mut self) {
        if !self.trim.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        let Some(task) = self.trim.take() else {
            return;
        };
        match task.await {
            Ok(Ok(emptied)) => {
                for (name, size) in emptied {
                    info!("emptied {name}, which had grown to {size}");
                }
            }
            Ok(Err(error)) => warn!("cannot check the shared caches: {error}"),
            Err(error) => warn!("checking the shared caches stopped: {error}"),
        }
    }

    async fn collect_reseeds(&mut self) {
        let finished: Vec<String> = self
            .reseeds
            .iter()
            .filter(|(_, task)| task.is_finished())
            .map(|(name, _)| name.clone())
            .collect();
        for name in finished {
            let Some(task) = self.reseeds.remove(&name) else {
                continue;
            };
            match task.await {
                Ok(Ok(output)) => info!("{name}: {}", output.trim().replace('\n', "; ")),
                Ok(Err(error)) => warn!("{name}: reseeding failed: {error}"),
                Err(error) => warn!("{name}: reseeding stopped: {error}"),
            }
        }
    }

    fn publish(&mut self) {
        let mode = self.control.mode();
        let profiles = self
            .config
            .profiles
            .iter()
            .map(|(name, profile)| api::ProfileStatus {
                name: name.clone(),
                image: image(&profile.image_stage),
                cpus: profile.cpus,
                memory: profile.memory.0,
                reseeding: self.reseeds.contains_key(name),
                slots: self.slots[name]
                    .iter()
                    .enumerate()
                    .map(|(index, slot)| api::SlotStatus {
                        slot: index + 1,
                        runner: slot.as_ref().map(|occupant| api::RunnerStatus {
                            repository: occupant.repository.clone(),
                            name: occupant.runner_name.clone(),
                            runner_id: occupant.runner_id,
                            container: occupant.container.clone(),
                            busy: occupant.busy,
                            started_at: unix_secs(occupant.started_at),
                        }),
                    })
                    .collect(),
            })
            .collect();
        let repositories = self
            .config
            .repositories
            .iter()
            .map(|repository| api::RepositoryStatus {
                name: repository.name.clone(),
                profile: repository.profile.clone(),
                labels: repository.labels.clone(),
                paused: self.control.is_paused(&repository.name),
                queued_jobs: self.repositories[&repository.name]
                    .queued
                    .iter()
                    .filter(|job| super::matches(job, &repository.labels))
                    .count(),
            })
            .collect();
        let status = api::Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            shutting_down: mode != Shutdown::No,
            budget: api::Budget {
                slice: self.config.host.slice.clone(),
                memory_max: self.memory_max,
                memory_in_use: self.memory_in_use(),
            },
            profiles,
            repositories,
        };
        self.status.send_replace(status);
    }
}
