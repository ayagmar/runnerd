//! The scheduling loop: polls GitHub, applies `plan`'s decisions with GitHub and Docker, removes each runner's
//! container when its job ends, keeps the mirror up while any runner runs, reseeds idle profiles and recovers
//! runners and containers a previous daemon left. It also feeds the job history: queued jobs, the job each runner
//! took (from `observe::capture`), steps from GitHub, mirror bytes, cache sizes and slot and budget changes. It
//! owns slot state; it never serves the API (it publishes `api::Status` and takes commands through `Control`) and
//! never logs the token or a JIT config.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{Decision, Profile, Repository, RunnerSlot, Slot, SlotRef, plan};
use crate::api;
use crate::cache::{self, Names, runner_mounts};
use crate::config::{ByteSize, Config};
use crate::docker::{self, Docker, DockerApi, RunnerSpec, is_not_found};
use crate::github::{self, Deleted, Github, GithubApi, QueuedJob};
use crate::isolation::{self, Isolation};
use crate::observe::capture::{self, Captured, Parser};
use crate::observe::metrics::{self, Paths, Sampler, Tracked};
use crate::observe::mirror::Leases;
use crate::store::{LogPosition, MirrorPosition, RunnerRecord, Store};
use crate::time;

/// How long the history waits for GitHub to report a job completed after its container is gone, before it records
/// the job as lost. GitHub fails a job whose runner vanished after about 10 minutes.
const LOST_AFTER: Duration = Duration::from_mins(15);

/// How long `release` waits for a runner's log capture to end once its container stopped.
const CAPTURE_DRAIN: Duration = Duration::from_secs(10);

/// How often the history drops what it no longer keeps.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// The image a profile's runners run.
pub fn image(stage: &str) -> String {
    format!("runnerd-runner:{stage}")
}

/// This machine as its runner names carry it: `runnerd-<id>-<host>-<profile>-<slot>-<secs>`. Only `runnerd-<id>-`
/// identifies the machine, so a changed host name still matches the runners started before the change, and two
/// machines with the same or overlapping host names never take each other's runners for their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    /// 8 hex digits: a hash of the systemd machine ID, never the ID itself, as machine-id(5) asks.
    id: String,
    /// The host name in lower case, letters, digits and dashes, at most 20 characters: for people reading names.
    host: String,
}

impl HostIdentity {
    /// This machine's identity, from `/etc/machine-id` and its host name.
    pub fn read() -> std::io::Result<HostIdentity> {
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let machine_id = std::fs::read_to_string("/etc/machine-id")
            .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("cannot read /etc/machine-id: {error}"),
                )
            })?;
        HostIdentity::new(&hostname, &machine_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "/etc/machine-id does not hold a machine ID",
            )
        })
    }

    /// The identity for a host name and a machine ID; `None` unless the ID is 32 hex digits.
    pub fn new(hostname: &str, machine_id: &str) -> Option<HostIdentity> {
        let machine_id = machine_id.trim();
        if machine_id.len() != 32 || !machine_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        // FNV-1a: stable across Rust releases, unlike std's hasher, so a new build recognizes the runners of the old.
        let hash = format!("runnerd:{}", machine_id.to_ascii_lowercase())
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        let host: String = hostname
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
        let host = host.trim_matches('-');
        Some(HostIdentity {
            id: format!("{:08x}", hash >> 32),
            host: if host.is_empty() { "host" } else { host }.to_owned(),
        })
    }

    /// What every runner name of this machine starts with. The ID's fixed length keeps it from starting another
    /// machine's names.
    pub fn prefix(&self) -> String {
        format!("runnerd-{}-", self.id)
    }

    /// The name of a runner, and of its container, started in `slot` (1-based) of `profile` at `secs`.
    pub fn runner_name(&self, profile: &str, slot: usize, secs: u64) -> String {
        format!("{}{}-{profile}-{slot}-{secs}", self.prefix(), self.host)
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
    /// The latest status, replaced after every tick.
    pub status: watch::Receiver<api::Status>,
    /// Pause, resume and shutdown requests.
    pub control: Arc<Control>,
    /// The job history.
    pub store: Store,
    /// Every cache volume of the config, with size 0: the API fills in the sizes it measured.
    pub caches: Vec<api::CacheVolume>,
}

/// Where the engine runs, besides its config and its GitHub and Docker clients.
#[derive(Debug)]
pub struct Setup {
    /// The names of what runnerd creates in Docker.
    pub names: Names,
    /// This machine, which names its runners, so a restarted daemon recognizes them.
    pub identity: HostIdentity,
    /// The slice's `MemoryMax`.
    pub memory_max: Option<u64>,
    /// The job history.
    pub store: Store,
    /// Where the cgroup and proc file systems are.
    pub paths: Paths,
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
    /// The job it took, once its log capture found it.
    job: Option<u64>,
}

#[derive(Debug)]
struct RepositoryState {
    queued: Vec<QueuedJob>,
    /// Whether the last poll of its queued jobs succeeded.
    listed: bool,
    held_until: Option<SystemTime>,
    /// Runners in a row that ended without taking a job.
    failures: u32,
}

/// A runner's log capture found its job.
#[derive(Debug)]
struct Found {
    runner: String,
    job: u64,
}

/// What `trim_shared` measured, and the shared caches it emptied with their sizes.
type Trimmed = (BTreeMap<String, u64>, Vec<(String, ByteSize)>);

/// The scheduler with its side effects, on the real GitHub and Docker unless a test passes fakes.
#[derive(Debug)]
pub struct Engine<G = Github, D = Docker> {
    config: Config,
    github: G,
    docker: Arc<D>,
    names: Names,
    isolation: &'static dyn Isolation,
    identity: HostIdentity,
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
    trim: Option<JoinHandle<Result<Trimmed, docker::Error>>>,
    mirror_up: bool,
    rate_limit_logged: bool,
    /// Runners without a container that GitHub still holds busy, already logged.
    busy_orphans: BTreeSet<u64>,
    status: watch::Sender<api::Status>,
    control: Arc<Control>,
    store: Store,
    paths: Paths,
    /// The containers the sampler reads.
    tracked: watch::Sender<Vec<Tracked>>,
    found: (mpsc::UnboundedSender<Found>, mpsc::UnboundedReceiver<Found>),
    captures: BTreeMap<String, JoinHandle<()>>,
    /// Who held which address, for the mirror's log.
    leases: Leases,
    /// How far the mirror's log was counted.
    mirror_until: MirrorPosition,
    /// Jobs recorded queued and not yet taken or closed, with their repository.
    queued_jobs: BTreeMap<u64, String>,
    /// Jobs whose runner is gone, waiting for GitHub to report them completed: repository, and since when.
    closing: BTreeMap<u64, (String, SystemTime)>,
    last_prune: Option<SystemTime>,
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

impl<G: GithubApi, D: DockerApi + 'static> Engine<G, D> {
    /// An engine for a validated config. `identity` names this machine's runners, so a restarted daemon recognizes
    /// them; `memory_max` is the slice's.
    pub fn new(config: Config, github: G, docker: D, setup: Setup) -> (Engine<G, D>, Handle) {
        let Setup {
            names,
            identity,
            memory_max,
            store,
            paths,
        } = setup;
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
                        listed: false,
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
        let caches = cache::volumes(&names, &config);
        let mut engine = Engine {
            config,
            github,
            docker: Arc::new(docker),
            names,
            isolation,
            runner_prefix: identity.prefix(),
            identity,
            memory_max,
            slots,
            repositories,
            reseeds: BTreeMap::new(),
            reseed_due: BTreeSet::new(),
            // So the caches are measured once at start.
            trim_due: true,
            trim: None,
            // Unknown: an earlier run may have left it up, so the first idle tick stops it.
            mirror_up: true,
            rate_limit_logged: false,
            busy_orphans: BTreeSet::new(),
            status,
            control: control.clone(),
            store: store.clone(),
            paths,
            tracked: watch::channel(Vec::new()).0,
            found: mpsc::unbounded_channel(),
            captures: BTreeMap::new(),
            leases: Leases::default(),
            mirror_until: MirrorPosition::default(),
            queued_jobs: BTreeMap::new(),
            closing: BTreeMap::new(),
            last_prune: None,
        };
        engine.publish();
        (
            engine,
            Handle {
                status: receiver,
                control,
                store,
                caches,
            },
        )
    }

    /// Runs until a shutdown request is carried out.
    pub async fn run(mut self) {
        tokio::spawn(metrics::run(
            Sampler::new(self.paths.clone(), &self.config.host.slice),
            self.tracked.subscribe(),
            self.store.clone(),
        ));
        // Until Docker answers: scheduling without knowing the containers already running would start a second
        // runner on a slot's volume and delete the running one's registration.
        while let Err(error) = self.recover().await {
            warn!("cannot list the containers an earlier run left; trying again in 5s: {error}");
            if self.control.mode() != Shutdown::No {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(5)) => {}
                () = self.control.wake.notified() => {}
            }
        }
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
        if let Err(error) = cache::mirror_down(&*self.docker, &self.names).await {
            warn!("cannot stop the mirror: {error}");
        }
        info!("stopped");
    }

    /// Takes over the seed containers a previous daemon left: a running one counts as its profile's reseed, so no
    /// runner of the profile starts while it rewrites the snapshot and the slots' layers.
    fn recover_reseeds(&mut self, containers: Vec<docker::Container>) {
        for container in containers {
            let Some(profile) = container.labels.get("runnerd.profile").cloned() else {
                continue;
            };
            // Only this daemon's own names: tests seed under other prefixes on the same Engine.
            if container.name != self.names.seed(&profile) || !self.slots.contains_key(&profile) {
                continue;
            }
            info!("{profile}: waiting for the reseed an earlier run started");
            let docker = self.docker.clone();
            let id = container.id.clone();
            self.reseeds.insert(
                profile,
                tokio::spawn(async move { cache::finish_reseed(&*docker, &id).await }),
            );
        }
    }

    /// Adopts the runner and seed containers a previous daemon left running, and removes the runners that
    /// stopped, or that run under this machine's names for no free slot. Their registrations, and any of this host's runners without a container, go at the first poll.
    /// Changes nothing unless Docker lists both kinds, so it can be tried again.
    async fn recover(&mut self) -> Result<(), docker::Error> {
        let seeds = self.docker.list("runnerd.seed").await?;
        let containers = self.docker.list("runnerd.job").await?;
        self.recover_history().await;
        self.recover_reseeds(seeds);
        for container in containers {
            // Runner containers on this Engine are this machine's whatever their name, which older builds or
            // another machine ID may have chosen: a running one is adopted when its labels name a free slot.
            let ours = container.name.starts_with(&self.runner_prefix);
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
                job: None,
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
                    *slot = Some(occupant.clone());
                    let slot = SlotRef {
                        profile,
                        index: index.unwrap_or(0),
                    };
                    self.observe(&slot, &occupant).await;
                }
                _ if !ours && container.running => {
                    warn!(
                        "leaving {}: a runner container not named by this machine, for no free slot",
                        container.name
                    );
                }
                _ => {
                    info!("removing {}, left by an earlier run", container.name);
                    if let Err(error) = self.docker.remove(&container.id).await {
                        warn!("cannot remove {}: {error}", container.name);
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads what the history left open: queued jobs to watch, and jobs whose runner is gone to close. Jobs of
    /// runners adopted later are found again by their log capture.
    async fn recover_history(&mut self) {
        match self.store.open_jobs().await {
            Ok(open) => {
                for job in open {
                    match job.state {
                        api::JobState::Queued => {
                            self.queued_jobs.insert(job.id, job.repository);
                        }
                        _ => {
                            self.closing
                                .insert(job.id, (job.repository, SystemTime::now()));
                        }
                    }
                }
            }
            Err(error) => warn!("cannot read the job history: {error}"),
        }
        match self.store.mirror_log_until().await {
            Ok(until) => self.mirror_until = until,
            Err(error) => warn!("cannot read the job history: {error}"),
        }
    }

    /// Starts watching a runner's container: records it, samples it, leases its address for the mirror's log, and
    /// captures its job and log.
    async fn observe(&mut self, slot: &SlotRef, occupant: &Occupant) {
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
    fn take_found(&mut self) {
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
        } else {
            for state in self.repositories.values_mut() {
                state.listed = false;
            }
        }
        self.take_found();
        self.follow_jobs().await;
        self.count_mirror_bytes().await;
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
        self.prune();
    }

    /// Records the queued jobs the repositories' runners may run, and closes those that left the queue for another
    /// machine's runner or were cancelled; asks GitHub for the steps of running jobs and whether the jobs of removed
    /// runners completed.
    async fn follow_jobs(&mut self) {
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
                if super::matches(job, &repository.labels) && !taken.contains(&job.id) {
                    self.store.queued(&repository.name, job);
                    self.queued_jobs
                        .entry(job.id)
                        .or_insert_with(|| repository.name.clone());
                }
            }
        }
        let left: Vec<(u64, String)> = self
            .queued_jobs
            .iter()
            .filter(|(id, repository)| {
                !taken.contains(*id)
                    && self.repositories.get(*repository).is_some_and(|state| {
                        state.listed && !state.queued.iter().any(|job| job.id == **id)
                    })
            })
            .map(|(id, repository)| (*id, repository.clone()))
            .collect();
        for (id, repository) in left {
            match self.github.job(&repository, id).await {
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
        for (repository, id) in running {
            match self.github.job(&repository, id).await {
                Ok(details) => self.store.details(details),
                Err(error) => warn!("{repository}: cannot read job {id}: {error}"),
            }
        }
        let closing: Vec<(u64, String, SystemTime)> = self
            .closing
            .iter()
            .map(|(id, (repository, since))| (*id, repository.clone(), *since))
            .collect();
        for (id, repository, since) in closing {
            match self.github.job(&repository, id).await {
                Ok(details) if details.status == "completed" => {
                    self.closing.remove(&id);
                    self.store.details(details);
                }
                Ok(_) | Err(_) if since.elapsed().is_ok_and(|waited| waited >= LOST_AFTER) => {
                    warn!("{repository}: job {id} lost: GitHub did not report it completed");
                    self.closing.remove(&id);
                    self.store.lost(id, time::ms(since));
                }
                Ok(details) => self.store.details(details),
                Err(error) => warn!("{repository}: cannot read job {id}: {error}"),
            }
        }
    }

    /// Counts what the mirror served each runner since the last count.
    async fn count_mirror_bytes(&mut self) {
        if !self.config.caches.mirror.enabled || !self.mirror_up {
            return;
        }
        let log = match self
            .docker
            .logs_since(&self.names.mirror(), self.mirror_until.at / 1_000_000_000)
            .await
        {
            Ok(log) => log,
            Err(error) if is_not_found(&error) => return,
            Err(error) => {
                warn!("cannot read the mirror's log: {error}");
                return;
            }
        };
        let (bytes, until) = self.leases.attribute(&log, self.mirror_until);
        if until != self.mirror_until {
            self.mirror_until = until;
            self.leases.forget_before(until.at / 1_000_000);
            self.store.mirror_bytes(bytes, until);
        }
    }

    fn prune(&mut self) {
        let now = SystemTime::now();
        if self.last_prune.is_some_and(|last| {
            now.duration_since(last)
                .is_ok_and(|since| since < PRUNE_EVERY)
        }) {
            return;
        }
        self.last_prune = Some(now);
        self.store.prune(time::ms(now), &self.config.history);
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
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
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
                    // GitHub lists a job queued for a few seconds after a runner took it: without this, a tick in
                    // between starts a second runner for it.
                    queued: state
                        .queued
                        .iter()
                        .filter(|job| !taken.contains(&job.id))
                        .cloned()
                        .collect(),
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

    /// Memory caps of the runners, the seed containers (which run with their profile's limits) and the mirror.
    fn memory_in_use(&self) -> u64 {
        let mirror = &self.config.caches.mirror;
        let mirror = if mirror.enabled { mirror.memory.0 } else { 0 };
        let profile_memory = |name: &String| {
            self.config
                .profiles
                .get(name)
                .map_or(0, |profile| profile.memory.0)
        };
        let runners: u64 = self
            .occupied()
            .map(|(slot, _)| profile_memory(&slot.profile))
            .sum();
        let reseeds: u64 = self.reseeds.keys().map(profile_memory).sum();
        runners + reseeds + mirror
    }

    async fn poll_queued(&mut self) {
        for repository in &self.config.repositories {
            let mut listed = false;
            let queued = match self.github.queued_jobs(&repository.name).await {
                Ok(queued) => {
                    self.rate_limit_logged = false;
                    listed = true;
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
                state.listed = listed;
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
                let held = self
                    .slots
                    .values_mut()
                    .flatten()
                    .flatten()
                    .find(|occupant| occupant.runner_id == runner.id);
                match held {
                    Some(occupant) => occupant.busy = runner.busy,
                    // Only this machine's names: other machines serve the same repositories.
                    None if !runner.name.starts_with(&self.runner_prefix) => {}
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
        // The capture ends with the container; its last lines must be stored before the job's record closes.
        self.end_capture(&occupant.runner_name).await;
        self.count_mirror_bytes().await;
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
        self.forget(occupant);
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

    /// Waits for a runner's log capture to end, which it does once the container stopped; stops it after
    /// `CAPTURE_DRAIN`.
    async fn end_capture(&mut self, runner: &str) {
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
    fn forget(&mut self, occupant: &Occupant) {
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
            self.closing.insert(job, (occupant.repository.clone(), now));
        }
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
            self.end_capture(&occupant.runner_name).await;
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
        let mirror = &self.config.caches.mirror;
        if mirror.enabled && !self.mirror_up {
            match cache::mirror_up(&*self.docker, &self.names, mirror, &self.config.host.slice)
                .await
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
                let occupant = Occupant {
                    repository: repository.to_owned(),
                    runner_name: name,
                    runner_id: runner.id,
                    container,
                    busy: false,
                    started_at: now,
                    job: None,
                };
                if let Some(entry) = self
                    .slots
                    .get_mut(&slot.profile)
                    .and_then(|slots| slots.get_mut(slot.index))
                {
                    *entry = Some(occupant.clone());
                }
                self.observe(slot, &occupant).await;
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
            match cache::mirror_down(&*self.docker, &self.names).await {
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
                cache::trim_shared(&*docker, &names, &caches).await
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
                &*docker,
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
            Ok(Ok((sizes, emptied))) => {
                let ours = self.names.network();
                self.store.cache_sizes(
                    time::now_ms(),
                    sizes
                        .into_iter()
                        .filter(|(name, _)| name.starts_with(&format!("{ours}-")))
                        .collect(),
                );
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
            let (output, error) = match task.await {
                Ok(Ok(output)) => {
                    info!("{name}: {}", output.trim().replace('\n', "; "));
                    (output, None)
                }
                Ok(Err(error)) => {
                    warn!("{name}: reseeding failed: {error}");
                    (String::new(), Some(error.to_string()))
                }
                Err(error) => {
                    warn!("{name}: reseeding stopped: {error}");
                    (String::new(), Some(error.to_string()))
                }
            };
            self.store.event(api::Event::CacheReseeded {
                profile: name,
                output: output.trim().to_owned(),
                error,
            });
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
        let previous = self.status.borrow().clone();
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
        self.announce_changes(&previous, &status);
        self.status.send_replace(status);
    }

    /// Announces the slots and the budget that changed between two statuses.
    fn announce_changes(&self, previous: &api::Status, status: &api::Status) {
        for profile in &status.profiles {
            let before = previous
                .profiles
                .iter()
                .find(|candidate| candidate.name == profile.name);
            for slot in &profile.slots {
                let unchanged = before
                    .and_then(|before| before.slots.iter().find(|old| old.slot == slot.slot))
                    .is_some_and(|old| old == slot);
                if !unchanged {
                    self.store.event(api::Event::SlotChanged {
                        profile: profile.name.clone(),
                        slot: slot.clone(),
                    });
                }
            }
        }
        if previous.budget != status.budget {
            self.store.event(api::Event::BudgetChanged {
                budget: status.budget.clone(),
            });
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
                    }
                }
                Captured::Lines { lines, position } => {
                    if let Some(job) = job {
                        store.log(job, lines, position, max_bytes);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
