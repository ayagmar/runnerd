//! The scheduling loop: polls GitHub, pulls the profiles' images, applies `plan`'s decisions with GitHub and
//! Docker, removes each runner's container when its job ends, keeps the mirror up while any runner runs, reseeds
//! idle profiles and recovers runners and containers a previous daemon left. It also feeds the job history: queued
//! jobs, the job each runner took (from `observe::capture`), steps from GitHub, mirror bytes, cache sizes and slot
//! and budget changes. It owns slot state; it never serves the API (it publishes `api::Status` and takes commands
//! through `Control`) and never logs the token or a JIT config.
//!
//! This module holds `Engine`'s state, its construction and its loop; the submodules each add one concern's
//! methods: `recover` (startup), `tick` (polling and planning), `runners` (a slot's runner from start to removal),
//! `images` (pulling the profiles' images), `caches` (reseeds, the mirror and the shared caches), `build_cache` (each
//! runner's side of the shared build cache and whether its job may write), `history` (what
//! the store records), `warm` (how many warm runners each repository wants), `control` (the API's commands) and
//! `status` (what it publishes).

mod admission;
mod build_cache;
mod caches;
mod control;
mod history;
mod images;
mod records;
mod recover;
mod runners;
mod status;
mod tick;
mod warm;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub use self::control::{
    Author, Command, Commands, Commit, Control, ControlError, RunningJobs, RunningSlots, Shutdown,
    Target, same_slice,
};
use self::history::{Closing, Found};
use self::images::Images;
pub use self::records::RecordSelection;
use super::{Hold, SlotRef};
use crate::admission::Key;
use crate::api;
use crate::buildcache::BuildCache;
use crate::cache::{self, Names, Trimmed};
use crate::config::Config;
use crate::docker::{self, Docker, DockerApi};
use crate::github::{Github, GithubApi, QueuedJob};
use crate::isolation::{self, Isolation};
use crate::observe::metrics::{self, Paths, Sampler, Tracked};
use crate::observe::mirror::Leases;
use crate::store::{Announced, MirrorPosition, Store};

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
    /// The records of failed jobs, replaced whenever one is made or removed.
    pub records: watch::Receiver<api::Records>,
    /// The shared build cache, for its size now.
    pub build_cache: Option<BuildCache>,
    /// What the slots' Docker Engines may keep together, in bytes, from the last measure (`cache::docker_disk`);
    /// `None` before the first. Auto sizing adds no slot without it (ADR 0090).
    pub disk: watch::Receiver<Option<u64>>,
}

impl Handle {
    /// Asks the engine to stop in `mode` and says how it stops now: as asked, unless a stop asked for earlier is more
    /// urgent, or the engine published that a `runnerd stop` leaves running jobs since Docker stopped answering it.
    /// `None` for `Shutdown::No` when nothing stops.
    pub fn shutdown(&self, mode: Shutdown) -> Option<api::Stopping> {
        let requested = self.control.shutdown(mode);
        let published = self.status.borrow().stopping;
        match (requested, published) {
            (Shutdown::Graceful, Some(api::Stopping::LeavesJobs)) => {
                Some(api::Stopping::LeavesJobs)
            }
            _ => requested.stopping(),
        }
    }
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
    /// The CPUs the slice may use, in millicores (`config::slice_cpus`); `None` admits no runner by CPU.
    pub cpus: Option<u64>,
    /// The job history.
    pub store: Store,
    /// The profiles and repositories as the last run announced them (`Store::announced`), so the first publish
    /// announces only the difference, including what the config no longer has.
    pub announced: Announced,
    /// The repositories recorded paused (`Store::paused_repositories`): they stay paused from the first tick.
    pub paused: Vec<String>,
    /// Where the cgroup and proc file systems are.
    pub paths: Paths,
    /// The shared build cache, unless the daemon could not open it.
    pub build_cache: Option<BuildCache>,
}

/// Why a runner starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// For a queued job.
    Job,
    /// As a warm runner, for jobs not queued yet.
    Warm,
}

/// Whether Docker answers a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockerInStop {
    /// It answers, if with errors, or no stop was asked for.
    Answers,
    /// It did not answer a call the stop needed: the stop calls it no more.
    Failed,
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
    /// Its container's CPU limit in millicores, which a reload does not change.
    cpus: u64,
    /// How it was admitted, with what it reserved of the budget, kept through a restart by its record; `None` for
    /// one whose record says nothing of it, which reserves its cap.
    admission: Option<api::Admission>,
    /// The job it took, once its log capture found it.
    job: Option<u64>,
    /// That job as admission tells jobs apart, once the history had its record.
    key: Option<Key>,
    /// The labels it registered with; `None` for one adopted from a container that does not say.
    labels: Option<Vec<String>>,
    /// The image it runs; `None` for one adopted from a container that does not say.
    image: Option<String>,
    /// The release whose container spec it runs; `None` for one adopted from a container of a release before the
    /// label.
    spec: Option<String>,
    /// Whether `observe` watches it: recorded, sampled and its job captured. Not while Docker fails to inspect its
    /// container, which each tick tries again.
    observed: bool,
}

impl Occupant {
    /// Whether it no longer is what `repository`'s runners in `profile` start as: other labels, or other limits than
    /// the profile's. An unknown label set counts as the same.
    fn outdated(
        &self,
        repository: &crate::config::Repository,
        profile: &crate::config::Profile,
    ) -> bool {
        let differ = self.labels.as_ref().is_some_and(|labels| {
            let set = |labels: &[String]| -> BTreeSet<String> {
                labels
                    .iter()
                    .map(|label| label.to_ascii_lowercase())
                    .collect()
            };
            set(labels) != set(&repository.labels)
        });
        differ || self.memory != profile.memory.0 || self.cpus != u64::from(profile.cpus) * 1000
    }

    /// Whether it runs another image than `profile` names. An unknown image counts as the same.
    fn other_image(&self, profile: &crate::config::Profile) -> bool {
        self.image
            .as_ref()
            .is_some_and(|image| *image != profile.image.reference())
    }

    /// Whether another release started it, with another container spec: one that does not say is older.
    fn other_spec(&self) -> bool {
        self.spec.as_deref() != Some(runners::THIS_SPEC)
    }

    /// What it reserves of the budget under measured admission: its cap if it has no admission.
    fn reserved(&self) -> u64 {
        self.admission
            .map_or(self.memory, |admission| admission.reserved)
    }

    /// The CPU it reserves, in millicores: its CPU limit unless it was admitted by CPU.
    fn reserved_cpu(&self) -> u64 {
        self.admission
            .and_then(|admission| admission.cpu)
            .map_or(self.cpus, |cpu| cpu.reserved)
    }

    fn running_job(&self) -> api::RunningJob {
        api::RunningJob {
            repository: self.repository.clone(),
            runner: self.runner_name.clone(),
            job: self.job,
        }
    }
}

/// A repository's state, shared by its pools but for their holds and what they wait for CPU.
#[derive(Debug, Default)]
struct RepositoryState {
    queued: Vec<QueuedJob>,
    /// Whether the last poll of its queued jobs succeeded.
    listed: bool,
    /// By the profile of each of its pools, that pool's runners held back after they failed: a broken pool does
    /// not hold back the others (ADR 0058).
    holds: BTreeMap<String, PoolHold>,
    /// By the profile of each of its pools, that pool's oldest job held for CPU that memory and a slot had room
    /// for, and since when, kept while that job stays queued: a tick held by memory or slots in between does not
    /// make it younger.
    cpu_wait: BTreeMap<String, CpuWait>,
    /// The profiles of its pools whose next job the last plan held for CPU.
    cpu_held: BTreeSet<String>,
}

/// A pool's runners held back after they failed, as `RepositoryState::holds` keeps them.
#[derive(Debug, Default)]
struct PoolHold {
    held: Option<Hold>,
    /// Runners in a row that ended without taking a job.
    failures: u32,
    /// Runners in a row that ended without a job after waiting `IDLE_TIMEOUT` for one.
    idle_exits: u32,
}

/// A job waiting for CPU, as `RepositoryState::cpu_wait` keeps it.
#[derive(Debug, Clone, Copy)]
struct CpuWait {
    /// The job's ID.
    job: u64,
    /// When a plan first held it for CPU.
    since: SystemTime,
}

/// How long a stop that detaches or is forced takes at most, from its request: what Docker and GitHub have not
/// answered by then is left to the next start (ADR 0091). Well within systemd's default stop timeout of 90s, which
/// a single Docker request (120s) could otherwise outlast.
pub const STOP_DEADLINE: Duration = Duration::from_secs(30);

/// The scheduler with its side effects, on the real GitHub and Docker unless a test passes fakes.
#[derive(Debug)]
pub struct Engine<G = Github, D = Docker> {
    config: Config,
    /// Shared with the listings a tick reads while it answers commands.
    github: Arc<G>,
    docker: Arc<D>,
    names: Names,
    isolation: &'static dyn Isolation,
    identity: HostIdentity,
    runner_prefix: String,
    memory_max: Option<u64>,
    /// The CPUs the slice may use, in millicores, read at start.
    cpus: Option<u64>,
    slots: BTreeMap<String, Vec<Option<Occupant>>>,
    repositories: BTreeMap<String, RepositoryState>,
    reseeds: BTreeMap<String, JoinHandle<Result<String, docker::Error>>>,
    /// The pulled images of the config's profiles, and the removal of those no profile runs.
    images: Images,
    /// The memory cap each running reseed's container started with, which a reload does not change.
    reseed_memory: BTreeMap<String, u64>,
    /// Profiles the API asked to reseed: they start nothing until their slots are free and the reseed ran.
    reseed_requested: BTreeSet<String>,
    /// Repositories a reload removed, polled until none of this machine's runners is registered on them.
    draining: BTreeSet<String>,
    /// Draining repositories whose last runners listing had none of this machine's runners. A runner can go unseen
    /// in one listing whose pages shift while they are read, so a repository stops draining only after two.
    drained_once: BTreeSet<String>,
    /// The runner and shell containers an earlier run left that recovery failed to remove, by ID with their names:
    /// each tick tries again.
    leftover: BTreeMap<String, String>,
    /// Profiles a reload removed, kept while their runners or reseed still run.
    retired: BTreeMap<String, crate::config::Profile>,
    /// Profiles whose slot 1 ran a job since their last reseed check.
    reseed_due: BTreeSet<String>,
    /// Profiles whose snapshot the last check of the caches measured over `snapshot_max`: due, and reseeded however
    /// little slot 1 changed.
    oversized: BTreeSet<String>,
    /// When each profile's last reseed finished: a check of the caches that started before then measured the old
    /// snapshot.
    reseeded_at: BTreeMap<String, SystemTime>,
    /// The job each slot ran last, which left its caches in the slot's layer: what `plan` matches queued jobs to
    /// (ADR 0080). A slot is absent once its last job is not known, or a reseed dropped its layer.
    ran: BTreeMap<SlotRef, Key>,
    /// What the slots' Docker Engines may keep together, from the last check of the caches (`cache::docker_disk`);
    /// `None` until one measured Docker's disk, when each image keeps its own default.
    docker_disk: watch::Sender<Option<u64>>,
    /// The measure of Docker's disk recovery started, in the background: Docker measures every volume.
    disk: Option<JoinHandle<Result<caches::Disk, docker::Error>>>,
    /// Whether a job ended since the shared caches were last checked.
    trim_due: bool,
    /// The running check of the shared caches: Docker takes many seconds to measure volumes.
    trim: Option<JoinHandle<Result<Trimmed, docker::Error>>>,
    /// When the last check of the shared caches started; the daemon's start counts as one, as the first runs then
    /// unless runners were adopted.
    checked_at: SystemTime,
    mirror_up: bool,
    /// Whether Docker stopped answering a stop: the stop then leaves the rest to the next start, as a detach does.
    docker_in_stop: DockerInStop,
    /// The memory cap the running mirror started with.
    mirror_memory: u64,
    rate_limit_logged: bool,
    /// Runners without a container that GitHub still holds busy, already logged.
    busy_orphans: BTreeSet<u64>,
    status: watch::Sender<api::Status>,
    /// The profiles as the last run announced them, until the first publish compares the profiles with them. Their
    /// slots are older than the `slot.changed` events clients have, so that publish announces every slot instead.
    announced_profiles: Option<Vec<api::ProfileStatus>>,
    control: Arc<Control>,
    commands: Commands,
    caches: watch::Sender<Vec<api::CacheVolume>>,
    store: Store,
    paths: Paths,
    /// The containers the sampler reads.
    tracked: watch::Sender<Vec<Tracked>>,
    found: (mpsc::UnboundedSender<Found>, mpsc::UnboundedReceiver<Found>),
    captures: BTreeMap<String, JoinHandle<history::Capture>>,
    /// The runners whose log capture broke off, until they are gone.
    recaptures: BTreeMap<String, history::Recapture>,
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
    /// The records of failed jobs and the shells open in them.
    records: records::RecordState,
    /// The predictor and the high-water mark of measured admission, and what admission saw of the host.
    measured: admission::Measured,
    /// What warm runners learned from the history, and the workflow runs seen lately.
    warm: warm::Warm,
    /// The shared build cache.
    build_cache: Option<BuildCache>,
    /// Each repository's default branch, once a job of it needed it: the branch whose builds write to the build
    /// cache when `[caches.build] branches` names none.
    default_branches: BTreeMap<String, build_cache::DefaultBranch>,
    /// Removed runners' build cache uploads held until GitHub decides whether their jobs may write, at most
    /// `build_cache::ENDED_WAIT`, by runner.
    ended_uploads: BTreeMap<String, build_cache::EndedUploads>,
    /// When each repository's visibility is next asked of GitHub, which the asking task updates; the store keeps
    /// the answers.
    visibility_due: Arc<Mutex<BTreeMap<String, SystemTime>>>,
    /// The task asking GitHub repositories' visibility, off the tick (`follow_visibility`).
    visibility_asking: Option<JoinHandle<()>>,
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// An engine for a validated config. `identity` names this machine's runners, so a restarted daemon recognizes
    /// them; `memory_max` is the slice's.
    #[allow(
        clippy::too_many_lines,
        reason = "one line per field of the engine's state, which split up would only be listed twice"
    )]
    pub fn new(config: Config, github: G, docker: D, setup: Setup) -> (Engine<G, D>, Handle) {
        let Setup {
            names,
            identity,
            memory_max,
            cpus,
            store,
            announced,
            paused,
            paths,
            build_cache,
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
            paused,
            store.clone(),
        );
        let control = Arc::new(control);
        let (status, receiver) = watch::channel(api::Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            shutting_down: false,
            stopping: None,
            budget: api::Budget {
                slice: config.host.slice.clone(),
                memory_max,
                ..api::Budget::default()
            },
            profiles: Vec::new(),
            // What clients resuming after the last run's events have, so the first publish announces the difference.
            repositories: announced.repositories,
            queued: Vec::new(),
        });
        let (caches, caches_receiver) = watch::channel(cache::volumes(&names, &config));
        let (records, records_receiver) = records::RecordState::new();
        let (docker_disk, disk_receiver) = watch::channel(None);
        let mut engine = Engine {
            isolation: isolation::backend(config.host.isolation),
            // Unknown: an earlier run may have left the mirror up, with the configured cap at best.
            mirror_memory: config.mirror_memory(),
            config,
            github: Arc::new(github),
            docker: Arc::new(docker),
            names,
            runner_prefix: identity.prefix(),
            identity,
            memory_max,
            cpus,
            slots,
            repositories,
            reseeds: BTreeMap::new(),
            images: Images::new(),
            reseed_memory: BTreeMap::new(),
            reseed_requested: BTreeSet::new(),
            retired: BTreeMap::new(),
            draining: BTreeSet::new(),
            drained_once: BTreeSet::new(),
            leftover: BTreeMap::new(),
            reseed_due: BTreeSet::new(),
            oversized: BTreeSet::new(),
            reseeded_at: BTreeMap::new(),
            ran: BTreeMap::new(),
            docker_disk,
            disk: None,
            // So the caches are measured once at start.
            trim_due: true,
            trim: None,
            checked_at: SystemTime::now(),
            // Unknown: an earlier run may have left it up, so the first idle tick stops it.
            mirror_up: true,
            docker_in_stop: DockerInStop::Answers,
            rate_limit_logged: false,
            busy_orphans: BTreeSet::new(),
            status,
            announced_profiles: Some(announced.profiles),
            control: control.clone(),
            commands,
            caches,
            store: store.clone(),
            paths,
            tracked: watch::channel(Vec::new()).0,
            found: mpsc::unbounded_channel(),
            captures: BTreeMap::new(),
            recaptures: BTreeMap::new(),
            leases: Leases::default(),
            mirror_until: MirrorPosition::default(),
            queued_jobs: BTreeMap::new(),
            cancelling: BTreeMap::new(),
            closing: BTreeMap::new(),
            last_prune: None,
            measured: admission::Measured::default(),
            warm: warm::Warm::default(),
            records,
            build_cache: build_cache.clone(),
            default_branches: BTreeMap::new(),
            ended_uploads: BTreeMap::new(),
            visibility_due: Arc::default(),
            visibility_asking: None,
        };
        engine.publish();
        (
            engine,
            Handle {
                status: receiver,
                control,
                store,
                caches: caches_receiver,
                records: records_receiver,
                build_cache,
                disk: disk_receiver,
            },
        )
    }

    /// Runs until a shutdown request is carried out, or `STOP_DEADLINE` after a stop that detaches or is forced
    /// began, whichever comes first. Either way it returns, so the daemon still closes its history.
    pub async fn run(mut self) {
        tokio::spawn(metrics::run(
            Sampler::new(self.paths.clone(), &self.config.host.slice),
            self.tracked.subscribe(),
            self.store.clone(),
        ));
        let control = self.control.clone();
        let deadline = async {
            control.hurried().await;
            tokio::time::sleep(STOP_DEADLINE).await;
        };
        tokio::select! {
            () = self.serve() => {}
            () = deadline => warn!(
                "stopped after {}s with Docker or GitHub still busy; the next start takes over what is left",
                STOP_DEADLINE.as_secs()
            ),
        }
    }

    /// Recovers, then schedules until a shutdown request is carried out.
    async fn serve(&mut self) {
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
        self.schedule().await;
    }

    /// Schedules until a shutdown request is carried out, from a recovered engine.
    async fn schedule(&mut self) {
        let mode = loop {
            let mode = self.stopping();
            if mode == Shutdown::Detach {
                self.remove_idle().await;
                self.collect_recordings().await;
            } else {
                self.tick(mode).await;
            }
            self.publish();
            if self.stopped(mode) {
                break mode;
            }
            // A stop asked for or changed meanwhile, as one Docker failed turns into a detach, goes on at once.
            if self.stopping() != mode {
                continue;
            }
            let interval = if mode == Shutdown::No {
                self.config.host.poll_interval
            } else {
                Duration::from_secs(5)
            };
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = self.control.wake.notified() => {}
                // A detaching stop changes nothing more: a command waiting for it fails once the engine is gone.
                Some(command) = self.commands.recv(), if mode != Shutdown::Detach => {
                    self.command(command).await;
                    self.publish();
                }
                Some(()) = self.records.finished.recv() => {
                    self.collect_recordings().await;
                    self.publish();
                }
            }
        };
        // A reseed that finished meanwhile says so; one still running is the next start's (`recover_reseeds`).
        self.collect_reseeds().await;
        if self.docker_in_stop == DockerInStop::Failed {
            info!("stopped; the next start removes or adopts what Docker did not answer for");
            return;
        }
        self.close_shells().await;
        if mode == Shutdown::Detach {
            let runners = self.occupied().count();
            info!("stopped, leaving {runners} busy runners and the mirror to the next start");
            return;
        }
        if let Err(error) = cache::mirror_down(&*self.docker, &self.names).await {
            warn!("cannot stop the mirror: {error}");
        }
        info!("stopped");
    }

    /// How the stop goes on: as requested, or as a detach once Docker stopped answering it.
    fn stopping(&self) -> Shutdown {
        match self.control.mode() {
            Shutdown::No => Shutdown::No,
            _ if self.docker_in_stop == DockerInStop::Failed => Shutdown::Detach,
            mode => mode,
        }
    }

    /// Whether a stop in `mode` is done: what it waits for has ended. A record being made is waited for; a reseed and
    /// a check of the caches are not, as the next start adopts the reseed and checks the caches first.
    fn stopped(&self, mode: Shutdown) -> bool {
        let idle = self.slots.values().flatten().all(Option::is_none);
        let recorded = self.records.recordings.is_empty();
        match mode {
            Shutdown::No => false,
            Shutdown::Detach => recorded,
            Shutdown::Graceful | Shutdown::Force => idle && recorded,
        }
    }

    /// Notes that Docker failed a call a stop needed. If it did not answer at all, as when it is gone during a
    /// machine's shutdown, the stop calls it no more and leaves the rest to the next start, which removes or adopts
    /// every container by its labels. An answer that is an error, or a timeout, is tried again as any failed call:
    /// `runnerd stop` must not leave runners behind for one busy moment of Docker's.
    fn docker_failed(&mut self, error: &docker::Error) {
        if self.control.mode() != Shutdown::No
            && self.docker_in_stop == DockerInStop::Answers
            && docker::is_unreachable(error)
        {
            warn!("Docker does not answer during the stop; leaving the rest to the next start");
            self.docker_in_stop = DockerInStop::Failed;
        }
    }

    /// Runs `work` as a task of its own, answering commands and publishing after each until it is done, so a command
    /// waits for a slow GitHub no longer than for the command before it. A tick follows each such command, as one
    /// follows a command answered between ticks. The task goes on while a command runs: a request's timeout runs on
    /// the clock, not on polls.
    async fn serving<T: Send + 'static>(
        &mut self,
        work: impl Future<Output = T> + Send + 'static,
    ) -> T {
        let mut task = tokio::spawn(work);
        loop {
            tokio::select! {
                biased;
                done = &mut task => match done.map_err(tokio::task::JoinError::try_into_panic) {
                    Ok(output) => return output,
                    // As if it had panicked here.
                    Err(Ok(panic)) => std::panic::resume_unwind(panic),
                    // Only while the runtime shuts down, which drops this task too.
                    Err(Err(_)) => std::future::pending::<()>().await,
                },
                Some(command) = self.commands.recv() => {
                    self.command(command).await;
                    self.publish();
                    // A tick follows it, as one follows a command answered between ticks: a config it replaced
                    // polls its repositories then, not a poll interval later.
                    self.control.wake.notify_one();
                }
            }
        }
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

    /// The runner in `slot`, if one is.
    fn occupant(&self, slot: &SlotRef) -> Option<&Occupant> {
        self.slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .and_then(Option::as_ref)
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
        // The volumes of the slots dropped go at the next check of the caches.
        let slots = &self.slots;
        self.ran.retain(|slot, _| {
            slots
                .get(&slot.profile)
                .is_some_and(|slots| slot.index < slots.len())
        });
        let (slots, reseeds) = (&self.slots, &self.reseeds);
        self.retired
            .retain(|name, _| slots.contains_key(name) || reseeds.contains_key(name));
    }
}

#[cfg(test)]
mod tests;
