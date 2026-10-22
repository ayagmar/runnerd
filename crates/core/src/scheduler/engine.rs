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
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{Decision, Profile, Repository, RunnerSlot, Slot, SlotRef, plan};
use crate::api;
use crate::cache::{self, Names, runner_mounts};
use crate::config::{ByteSize, Config};
use crate::docker::{self, Docker, DockerApi, RunnerSpec, RunnerState, is_not_found};
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

/// How long the runner of a run a forced pause cancelled gets to report its job cancelled and exit, before it is
/// removed: GitHub completes a cancelled run only once its runner reports it, which took 40s in a test.
const CANCEL_GRACE: Duration = Duration::from_secs(90);

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

/// Why the engine refused or failed a command from the API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// No such repository, profile or cache.
    #[error("{0}")]
    NotFound(String),
    /// Runners use what the command would change.
    #[error("{message}")]
    Busy {
        /// Names the runners and their jobs.
        message: String,
        /// The runners in the way.
        jobs: Vec<api::RunningJob>,
    },
    /// The command conflicts with one already in progress.
    #[error("{0}")]
    Conflict(String),
    /// The command cannot apply to this target.
    #[error("{0}")]
    Invalid(String),
    /// Docker or GitHub failed.
    #[error("{0}")]
    Failed(String),
    /// The engine stopped.
    #[error("runnerd is stopping")]
    Stopped,
}

/// A command the engine carries out between ticks, with where its answer goes.
#[derive(Debug)]
pub enum Command {
    /// Pause a repository; with `force`, cancel its running jobs.
    Pause {
        /// `owner/name`.
        repository: String,
        /// Cancel running jobs instead of refusing.
        force: bool,
        /// The answer.
        reply: oneshot::Sender<Result<api::Paused, ControlError>>,
    },
    /// Make slot 1's layers the profile's snapshot once its slots are free.
    Reseed {
        /// The profile.
        profile: String,
        /// The answer.
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
    /// Remove a slot's volume or a shared cache's, unless a runner uses it.
    CleanCache {
        /// The volume.
        name: String,
        /// The answer.
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
    /// Run with a new, validated config.
    Reload {
        /// The config.
        config: Box<Config>,
        /// The `MemoryMax` of the slice it names.
        memory_max: Option<u64>,
        /// Answered once the engine runs with it.
        reply: oneshot::Sender<()>,
    },
}

/// The engine's end of `Control`'s commands.
#[derive(Debug)]
pub struct Commands(mpsc::UnboundedReceiver<Command>);

impl Commands {
    /// The next command; `None` once `Control` is gone.
    pub async fn recv(&mut self) -> Option<Command> {
        self.0.recv().await
    }
}

/// Commands from the API and signals to the loop.
#[derive(Debug)]
pub struct Control {
    repositories: Mutex<BTreeSet<String>>,
    paused: Mutex<BTreeSet<String>>,
    shutdown: Mutex<Shutdown>,
    wake: Notify,
    commands: mpsc::UnboundedSender<Command>,
}

impl Control {
    /// Controls for the configured repositories, and the commands they send the engine.
    pub fn new(repositories: impl IntoIterator<Item = String>) -> (Control, Commands) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let control = Control {
            repositories: Mutex::new(repositories.into_iter().collect()),
            paused: Mutex::new(BTreeSet::new()),
            shutdown: Mutex::new(Shutdown::No),
            wake: Notify::new(),
            commands,
        };
        (control, Commands(receiver))
    }

    /// Whether a shutdown was requested, and how.
    pub fn mode(&self) -> Shutdown {
        *lock(&self.shutdown)
    }

    /// Pauses or resumes a repository whatever it runs; `false` if it is not configured.
    pub fn set_paused(&self, repository: &str, paused: bool) -> bool {
        if !lock(&self.repositories).contains(repository) {
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

    /// Replaces the configured repositories; the removed ones are no longer paused.
    fn set_repositories(&self, repositories: BTreeSet<String>) {
        lock(&self.paused).retain(|repository| repositories.contains(repository));
        *lock(&self.repositories) = repositories;
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

    async fn send<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, ControlError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| ControlError::Stopped)?;
        answer.await.map_err(|_| ControlError::Stopped)
    }

    /// Pauses a repository. Without `force`, refuses while it runs a job; with it, cancels the runs of its running
    /// jobs on GitHub, whose runners are removed once they report it or `CANCEL_GRACE` later, and removes its other
    /// runners at once.
    pub async fn pause(&self, repository: &str, force: bool) -> Result<api::Paused, ControlError> {
        self.send(|reply| Command::Pause {
            repository: repository.to_owned(),
            force,
            reply,
        })
        .await?
    }

    /// Reseeds a profile as soon as its slots are free; none of them starts a runner until then.
    pub async fn reseed(&self, profile: &str) -> Result<(), ControlError> {
        self.send(|reply| Command::Reseed {
            profile: profile.to_owned(),
            reply,
        })
        .await?
    }

    /// Removes a slot's volume or a shared cache's, unless a runner uses it.
    pub async fn clean_cache(&self, name: &str) -> Result<(), ControlError> {
        self.send(|reply| Command::CleanCache {
            name: name.to_owned(),
            reply,
        })
        .await?
    }

    /// Runs with a validated config from now on. Running jobs keep running: removed profiles and slots go as
    /// their runners finish.
    pub async fn reload(
        &self,
        config: Config,
        memory_max: Option<u64>,
    ) -> Result<(), ControlError> {
        self.send(|reply| Command::Reload {
            config: Box::new(config),
            memory_max,
            reply,
        })
        .await
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
    /// Every cache volume of the config, with size 0: the API fills in the sizes it measured. Replaced when the
    /// config is.
    pub caches: watch::Receiver<Vec<api::CacheVolume>>,
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
    /// Its container's memory cap, which a reload does not change.
    memory: u64,
    /// The job it took, once its log capture found it.
    job: Option<u64>,
}

impl Occupant {
    fn running_job(&self) -> api::RunningJob {
        api::RunningJob {
            repository: self.repository.clone(),
            runner: self.runner_name.clone(),
            job: self.job,
        }
    }
}

/// `jobs` for people to read: `job 1 on runner-a, runner-b`.
fn list(jobs: &[api::RunningJob]) -> String {
    jobs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Default)]
struct RepositoryState {
    queued: Vec<QueuedJob>,
    /// Whether the last poll of its queued jobs succeeded.
    listed: bool,
    held_until: Option<SystemTime>,
    /// Runners in a row that ended without taking a job.
    failures: u32,
}

/// A job whose runner is gone, waiting for GitHub to report it completed.
#[derive(Debug, Clone)]
struct Closing {
    repository: String,
    /// When the runner was found gone, on Tokio's clock, which tests can advance.
    since: tokio::time::Instant,
    /// The same, in milliseconds since the epoch.
    since_ms: u64,
}

impl Closing {
    fn now(repository: &str) -> Closing {
        Closing {
            repository: repository.to_owned(),
            since: tokio::time::Instant::now(),
            since_ms: time::now_ms(),
        }
    }
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
    /// The memory cap each running reseed's container started with, which a reload does not change.
    reseed_memory: BTreeMap<String, u64>,
    /// Profiles the API asked to reseed: they start nothing until their slots are free and the reseed ran.
    reseed_requested: BTreeSet<String>,
    /// Repositories a reload removed, polled until none of this machine's runners is registered on them.
    draining: BTreeSet<String>,
    /// Profiles a reload removed, kept while their runners or reseed still run.
    retired: BTreeMap<String, crate::config::Profile>,
    /// Profiles whose slot 1 ran a job since their last reseed check.
    reseed_due: BTreeSet<String>,
    /// Whether a job ended since the shared caches were last checked.
    trim_due: bool,
    /// The running check of the shared caches: Docker takes many seconds to measure volumes.
    trim: Option<JoinHandle<Result<Trimmed, docker::Error>>>,
    mirror_up: bool,
    /// The memory cap the running mirror started with.
    mirror_memory: u64,
    rate_limit_logged: bool,
    /// Runners without a container that GitHub still holds busy, already logged.
    busy_orphans: BTreeSet<u64>,
    status: watch::Sender<api::Status>,
    control: Arc<Control>,
    commands: Commands,
    caches: watch::Sender<Vec<api::CacheVolume>>,
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
    /// Runners of runs a forced pause cancelled, with when they are removed if they still run.
    cancelling: BTreeMap<String, tokio::time::Instant>,
    /// Jobs whose runner is gone, waiting for GitHub to report them completed.
    closing: BTreeMap<u64, Closing>,
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
            .map(|repository| (repository.name.clone(), RepositoryState::default()))
            .collect();
        let (control, commands) = Control::new(
            config
                .repositories
                .iter()
                .map(|repository| repository.name.clone()),
        );
        let control = Arc::new(control);
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
        let (caches, caches_receiver) = watch::channel(cache::volumes(&names, &config));
        // Unknown: an earlier run may have left the mirror up, with the configured cap at best.
        let mirror_memory = if config.caches.mirror.enabled {
            config.caches.mirror.memory.0
        } else {
            0
        };
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
            reseed_memory: BTreeMap::new(),
            reseed_requested: BTreeSet::new(),
            retired: BTreeMap::new(),
            draining: BTreeSet::new(),
            reseed_due: BTreeSet::new(),
            // So the caches are measured once at start.
            trim_due: true,
            trim: None,
            // Unknown: an earlier run may have left it up, so the first idle tick stops it.
            mirror_up: true,
            mirror_memory,
            rate_limit_logged: false,
            busy_orphans: BTreeSet::new(),
            status,
            control: control.clone(),
            commands,
            caches,
            store: store.clone(),
            paths,
            tracked: watch::channel(Vec::new()).0,
            found: mpsc::unbounded_channel(),
            captures: BTreeMap::new(),
            leases: Leases::default(),
            mirror_until: MirrorPosition::default(),
            queued_jobs: BTreeMap::new(),
            cancelling: BTreeMap::new(),
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
                caches: caches_receiver,
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
                Some(command) = self.commands.recv() => {
                    self.command(command).await;
                    self.publish();
                }
            }
        }
        if let Err(error) = cache::mirror_down(&*self.docker, &self.names).await {
            warn!("cannot stop the mirror: {error}");
        }
        info!("stopped");
    }

    /// Takes over the seed containers a previous daemon left: a running one counts as its profile's reseed, so no
    /// runner of the profile starts while it rewrites the snapshot and the slots' layers.
    async fn recover_reseeds(&mut self, containers: Vec<docker::Container>) {
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
            let memory = self.running_cap(&container.id, &profile).await;
            self.reseed_memory.insert(profile.clone(), memory);
            self.reseeds.insert(
                profile,
                tokio::spawn(async move { cache::finish_reseed(&*docker, &id).await }),
            );
        }
    }

    /// The memory cap a container left by an earlier daemon runs with, which a reload since may have changed in the
    /// config; the profile's configured cap only when Docker cannot tell.
    async fn running_cap(&self, container: &str, profile: &str) -> u64 {
        match self.docker.inspect(container).await {
            Ok(RunnerState {
                memory: Some(memory),
                ..
            }) => memory,
            _ => self
                .config
                .profiles
                .get(profile)
                .map_or(0, |settings| settings.memory.0),
        }
    }

    /// Adopts the runner and seed containers a previous daemon left running, and removes the runners that
    /// stopped, or that run under this machine's names for no free slot. Their registrations, and any of this host's runners without a container, go at the first poll.
    /// Changes nothing unless Docker lists both kinds, so it can be tried again.
    async fn recover(&mut self) -> Result<(), docker::Error> {
        let seeds = self.docker.list("runnerd.seed").await?;
        let containers = self.docker.list("runnerd.job").await?;
        self.recover_reseeds(seeds).await;
        // A reload may have lowered the configured cap since the mirror started.
        match self.docker.inspect(&self.names.mirror()).await {
            Ok(state) if state.running => {
                self.mirror_memory = state.memory.unwrap_or(self.mirror_memory);
            }
            Ok(_) => self.mirror_memory = 0,
            Err(error) if is_not_found(&error) => self.mirror_memory = 0,
            // Unknown: the configured cap.
            Err(_) => {}
        }
        // Runner containers still running once recovery is done.
        let mut running = BTreeSet::new();
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
                memory: self.running_cap(&container.id, &profile).await,
                // Its capture reports it again; until then, or if it cannot, the history knows it.
                job: self.store.runner_job(&container.name).await.ok().flatten(),
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
                    running.insert(occupant.runner_name.clone());
                    let slot = SlotRef {
                        profile,
                        index: index.unwrap_or(0),
                    };
                    self.observe(&slot, &occupant).await;
                }
                _ if !ours && container.running => {
                    running.insert(container.name.clone());
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
        self.recover_history(&running).await;
        Ok(())
    }

    /// Reads what the history left open: queued jobs to watch, and jobs to close. A job whose runner's container
    /// is still `running` is left to run: `forget` closes it once its container is gone. Any other lost its
    /// container while no daemon watched: its runner is recorded removed now, unless it was before.
    async fn recover_history(&mut self, running: &BTreeSet<String>) {
        match self.store.open_jobs().await {
            Ok(open) => {
                for job in open {
                    if job.state == api::JobState::Queued {
                        self.queued_jobs.insert(job.id, job.repository);
                        continue;
                    }
                    if let Some(runner) = &job.runner {
                        if running.contains(runner) {
                            continue;
                        }
                        self.store.runner_removed(runner, time::now_ms());
                    }
                    self.closing.insert(job.id, Closing::now(&job.repository));
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
        self.kill_cancelled().await;
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
        let closing: Vec<(u64, Closing)> = self
            .closing
            .iter()
            .map(|(id, closing)| (*id, closing.clone()))
            .collect();
        for (
            id,
            Closing {
                repository,
                since,
                since_ms,
            },
        ) in closing
        {
            match self.github.job(&repository, id).await {
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
        // Past these lines only once they are stored: if not, the next count reads them again.
        if until != self.mirror_until && self.store.mirror_bytes(bytes, until).await {
            self.mirror_until = until;
            self.leases.forget_before(until.at / 1_000_000);
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
                let slots = &self.slots[name];
                // A reload may have left fewer slots than runners: free slots count only up to the configured number.
                let mut free = (profile.slots as usize)
                    .saturating_sub(slots.iter().filter(|slot| slot.is_some()).count());
                let slots = slots
                    .iter()
                    .map(|slot| match slot {
                        None if free > 0 => {
                            free -= 1;
                            Slot::Free
                        }
                        None => Slot::Closed,
                        Some(occupant) => Slot::Runner(RunnerSlot {
                            repository: occupant.repository.clone(),
                            busy: occupant.busy,
                            started_at: occupant.started_at,
                        }),
                    })
                    .collect();
                let profile = Profile {
                    memory: profile.memory.0,
                    reseeding: self.is_reseeding(name),
                    slots,
                };
                (name.clone(), profile)
            })
            .collect();
        let free_memory = self
            .memory_max
            .map(|max| max.saturating_sub(self.memory_in_use()));
        let mut decisions = plan(&repositories, &profiles, free_memory, SystemTime::now());
        // Waiting runners a reload left outside the config: their repository is gone or uses another profile now.
        decisions.extend(
            self.occupied()
                .filter(|(slot, occupant)| {
                    !occupant.busy
                        && occupant.job.is_none()
                        && !self.config.repositories.iter().any(|repository| {
                            repository.name == occupant.repository
                                && repository.profile == slot.profile
                        })
                })
                .map(|(slot, _)| Decision::Stop { slot }),
        );
        decisions
    }

    /// Memory caps of the runners, the seed containers (which run with their profile's limits) and the mirror.
    fn memory_in_use(&self) -> u64 {
        // The configured mirror is reserved for; a running one keeps the cap it started with until it stops.
        let mirror = &self.config.caches.mirror;
        let configured = if mirror.enabled { mirror.memory.0 } else { 0 };
        let running = if self.mirror_up {
            self.mirror_memory
        } else {
            0
        };
        let runners: u64 = self.occupied().map(|(_, occupant)| occupant.memory).sum();
        let reseeds: u64 = self.reseed_memory.values().sum();
        runners + reseeds + configured.max(running)
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
        let names: Vec<String> = self
            .repositories
            .keys()
            .chain(&self.draining)
            .cloned()
            .collect();
        for repository in names {
            // Whether this machine still has a runner registered on it.
            let mut left = false;
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
                    Some(occupant) => {
                        occupant.busy = runner.busy;
                        left = true;
                    }
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
                            left = true;
                            if self.busy_orphans.insert(runner.id) {
                                warn!(
                                    "{repository}: {} has no container, but GitHub says it runs a job; \
                                     deleting it once GitHub lets go",
                                    runner.name
                                );
                            }
                        }
                        Err(error) => {
                            left = true;
                            warn!("{repository}: cannot delete {}: {error}", runner.name);
                        }
                    },
                }
            }
            if !left {
                self.draining.remove(&repository);
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
        self.drop_closed_slots();
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
            self.closing.insert(job, Closing::now(&occupant.repository));
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
            self.drop_closed_slots();
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
                Ok(()) => {
                    self.mirror_up = true;
                    self.mirror_memory = mirror.memory.0;
                }
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
                    memory: profile.memory.0,
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
                .union(&self.reseed_requested)
                .filter(|name| {
                    self.slots
                        .get(*name)
                        .is_some_and(|slots| slots.iter().all(Option::is_none))
                })
                .cloned()
                .collect();
            for name in due {
                self.reseed_due.remove(&name);
                let requested = self.reseed_requested.remove(&name);
                self.spawn_reseed(name, requested);
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

    /// Rebuilds a profile's snapshot in the background; its slots start nothing until it is done. A `requested`
    /// reseed runs however little slot 1 changed.
    fn spawn_reseed(&mut self, name: String, requested: bool) {
        let Some(profile) = self.config.profiles.get(&name) else {
            return;
        };
        let docker = self.docker.clone();
        let names = self.names.clone();
        let image = image(&profile.image_stage);
        let slice = self.config.host.slice.clone();
        let (slots, cpus, memory) = (profile.slots as usize, profile.cpus, profile.memory);
        let min_changes = if requested {
            ByteSize(0)
        } else {
            self.config.caches.reseed_at
        };
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
        self.reseed_memory.insert(name.clone(), memory.0);
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
            self.reseed_memory.remove(&name);
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
        self.drop_closed_slots();
    }

    /// Whether a profile's snapshot is being rebuilt, or will be once its slots are free.
    fn is_reseeding(&self, profile: &str) -> bool {
        self.reseeds.contains_key(profile) || self.reseed_requested.contains(profile)
    }

    /// Drops the free slots past each profile's configured number, and the profiles a reload removed once nothing
    /// runs in them.
    fn drop_closed_slots(&mut self) {
        for (name, slots) in &mut self.slots {
            let count = self
                .config
                .profiles
                .get(name)
                .map_or(0, |profile| profile.slots as usize);
            while slots.len() > count && slots.last().is_some_and(Option::is_none) {
                slots.pop();
            }
        }
        let profiles = &self.config.profiles;
        self.slots
            .retain(|name, slots| !slots.is_empty() || profiles.contains_key(name));
        let (slots, reseeds) = (&self.slots, &self.reseeds);
        self.retired
            .retain(|name, _| slots.contains_key(name) || reseeds.contains_key(name));
    }

    /// Carries out a command from the API.
    async fn command(&mut self, command: Command) {
        match command {
            Command::Pause {
                repository,
                force,
                reply,
            } => {
                let _ = reply.send(self.pause(&repository, force).await);
            }
            Command::Reseed { profile, reply } => {
                let _ = reply.send(self.request_reseed(&profile));
            }
            Command::CleanCache { name, reply } => {
                let _ = reply.send(self.clean_cache(&name).await);
            }
            Command::Reload {
                config,
                memory_max,
                reply,
            } => {
                self.reload(*config, memory_max);
                let _ = reply.send(());
            }
        }
    }

    /// Pauses a repository. A runner that holds a job, or that GitHub reports busy, refuses it unless `force`;
    /// with `force`, the runs of the repository's jobs are cancelled, and its runners removed once they exit or
    /// `CANCEL_GRACE` later; a runner whose run cannot be cancelled is removed at once.
    async fn pause(&mut self, repository: &str, force: bool) -> Result<api::Paused, ControlError> {
        if !self.repositories.contains_key(repository) {
            return Err(ControlError::NotFound(format!(
                "{repository} is not a configured repository"
            )));
        }
        self.take_found();
        let runners: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .filter(|(_, occupant)| occupant.repository == repository)
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        let jobs: Vec<api::RunningJob> = runners
            .iter()
            .filter(|(_, occupant)| occupant.busy || occupant.job.is_some())
            .map(|(_, occupant)| occupant.running_job())
            .collect();
        if !force && !jobs.is_empty() {
            return Err(ControlError::Busy {
                message: format!(
                    "{repository} runs {}; pause with force to cancel it",
                    list(&jobs)
                ),
                jobs,
            });
        }
        self.control.set_paused(repository, true);
        info!("{repository}: paused");
        let mut stopped = Vec::new();
        if !force {
            return Ok(api::Paused { stopped });
        }
        for (slot, occupant) in runners {
            let cancelled = if let Some(job) = occupant.job {
                self.cancel(repository, job).await
            } else {
                if occupant.busy {
                    warn!(
                        "{repository}: {} runs a job its log has not named yet; GitHub fails it once the runner is gone",
                        occupant.runner_name
                    );
                }
                false
            };
            if cancelled {
                // `collect_finished` removes it once it exits, `kill_cancelled` if it does not.
                self.cancelling.insert(
                    occupant.runner_name.clone(),
                    tokio::time::Instant::now() + CANCEL_GRACE,
                );
            } else {
                self.stop(&slot, true).await;
                if self
                    .occupied()
                    .any(|(_, candidate)| candidate.runner_name == occupant.runner_name)
                {
                    continue;
                }
            }
            stopped.push(occupant.running_job());
        }
        Ok(api::Paused { stopped })
    }

    /// Removes the runners of cancelled runs that did not exit within `CANCEL_GRACE`, every tick until they are gone.
    async fn kill_cancelled(&mut self) {
        let now = tokio::time::Instant::now();
        let running: BTreeSet<String> = self
            .occupied()
            .map(|(_, occupant)| occupant.runner_name.clone())
            .collect();
        self.cancelling.retain(|name, _| running.contains(name));
        let overdue: Vec<(SlotRef, String)> = self
            .occupied()
            .filter(|(_, occupant)| {
                self.cancelling
                    .get(&occupant.runner_name)
                    .is_some_and(|deadline| *deadline <= now)
            })
            .map(|(slot, occupant)| (slot, occupant.runner_name.clone()))
            .collect();
        for (slot, name) in overdue {
            warn!(
                "{name} did not exit {}s after its run was cancelled",
                CANCEL_GRACE.as_secs()
            );
            // The deadline stays until the runner is gone, so a failed removal is tried again next tick.
            self.stop(&slot, true).await;
        }
    }

    /// Cancels the run of a job; whether GitHub accepted it.
    async fn cancel(&self, repository: &str, job: u64) -> bool {
        let run = match self.github.job(repository, job).await {
            Ok(details) => details.run_id,
            Err(error) => {
                warn!("{repository}: cannot read job {job} to cancel its run: {error}");
                return false;
            }
        };
        match self.github.cancel_run(repository, run).await {
            Ok(()) => {
                info!("{repository}: cancelled run {run} of job {job}");
                true
            }
            Err(error) => {
                warn!("{repository}: cannot cancel run {run}: {error}");
                false
            }
        }
    }

    /// Asks for a profile's reseed: none of its slots starts a runner until they are free and it ran.
    fn request_reseed(&mut self, profile: &str) -> Result<(), ControlError> {
        if !self.config.profiles.contains_key(profile) {
            return Err(ControlError::NotFound(format!(
                "{profile} is not a configured profile"
            )));
        }
        if self.is_reseeding(profile) {
            return Err(ControlError::Conflict(format!(
                "{profile} is reseeding already"
            )));
        }
        info!("{profile}: reseeding once its slots are free");
        self.reseed_requested.insert(profile.to_owned());
        Ok(())
    }

    /// Removes a slot's volume or a shared cache's, unless a runner or a reseed uses it. Snapshots are rebuilt by
    /// reseeding instead: slot layers sit on top of them.
    async fn clean_cache(&mut self, name: &str) -> Result<(), ControlError> {
        let Some(volume) = cache::volumes(&self.names, &self.config)
            .into_iter()
            .find(|volume| volume.name == name)
        else {
            return Err(ControlError::NotFound(format!(
                "{name} is not a cache volume of this config"
            )));
        };
        let users: Vec<api::RunningJob> = match &volume.kind {
            api::CacheKind::Snapshot { profile } => {
                return Err(ControlError::Invalid(format!(
                    "{name} is {profile}'s snapshot, which every slot's layers sit on; reseed {profile} to rebuild it"
                )));
            }
            api::CacheKind::Slot { profile, slot } => {
                if self.reseeds.contains_key(profile) {
                    return Err(ControlError::Conflict(format!(
                        "{profile} is reseeding, which writes {name}"
                    )));
                }
                self.occupied()
                    .filter(|(at, _)| at.profile == *profile && at.index + 1 == *slot)
                    .map(|(_, occupant)| occupant.running_job())
                    .collect()
            }
            api::CacheKind::Pnpm | api::CacheKind::Maven | api::CacheKind::Mirror => self
                .occupied()
                .map(|(_, occupant)| occupant.running_job())
                .collect(),
        };
        if !users.is_empty() {
            return Err(ControlError::Busy {
                message: format!("{name} is in use by {}", list(&users)),
                jobs: users,
            });
        }
        if volume.kind == api::CacheKind::Mirror && self.mirror_up {
            cache::mirror_down(&*self.docker, &self.names)
                .await
                .map_err(|error| {
                    ControlError::Failed(format!("cannot stop the mirror: {error}"))
                })?;
            self.mirror_up = false;
        }
        self.docker
            .remove_volume(name)
            .await
            .map_err(|error| ControlError::Failed(format!("cannot remove {name}: {error}")))?;
        info!("removed {name}");
        // Measured again once idle.
        self.trim_due = true;
        Ok(())
    }

    /// Runs with a new, validated config. Running jobs keep running: a removed profile, or slots past a lowered
    /// number, stay until their runners finish, and no runner starts in a profile while it has as many as its
    /// slots. Runners started from now on get the new settings.
    fn reload(&mut self, config: Config, memory_max: Option<u64>) {
        let mut old = std::mem::replace(&mut self.config, config);
        for (name, profile) in &self.config.profiles {
            let slots = self.slots.entry(name.clone()).or_default();
            if slots.len() < profile.slots as usize {
                slots.resize(profile.slots as usize, None);
            }
            self.retired.remove(name);
        }
        for (name, profile) in std::mem::take(&mut old.profiles) {
            if !self.config.profiles.contains_key(&name) {
                self.retired.insert(name, profile);
            }
        }
        self.drop_closed_slots();
        let names: BTreeSet<String> = self
            .config
            .repositories
            .iter()
            .map(|repository| repository.name.clone())
            .collect();
        let removed: Vec<String> = self
            .repositories
            .keys()
            .filter(|name| !names.contains(*name))
            .cloned()
            .collect();
        self.draining.extend(removed);
        self.draining.retain(|name| !names.contains(name));
        self.repositories.retain(|name, _| names.contains(name));
        for name in &names {
            self.repositories.entry(name.clone()).or_default();
        }
        self.control.set_repositories(names);
        let profiles = &self.config.profiles;
        self.reseed_requested
            .retain(|name| profiles.contains_key(name));
        self.isolation = isolation::backend(self.config.host.isolation);
        self.memory_max = memory_max;
        self.caches
            .send_replace(cache::volumes(&self.names, &self.config));
        info!("reloaded the config");
    }

    fn publish(&mut self) {
        let mode = self.control.mode();
        let profiles = self
            .config
            .profiles
            .iter()
            .chain(&self.retired)
            .map(|(name, profile)| api::ProfileStatus {
                name: name.clone(),
                image: image(&profile.image_stage),
                cpus: profile.cpus,
                memory: profile.memory.0,
                reseeding: self.is_reseeding(name),
                slots: self
                    .slots
                    .get(name)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    // Slots a reload removed show while their runner runs.
                    .filter(|(index, slot)| {
                        slot.is_some()
                            || self
                                .config
                                .profiles
                                .get(name)
                                .is_some_and(|profile| *index < profile.slots as usize)
                    })
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

#[cfg(test)]
mod tests;
