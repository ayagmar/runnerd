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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub use self::control::{
    Command, Commands, Commit, Control, ControlError, RunningJobs, RunningSlots, Shutdown, Target,
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
}

impl Occupant {
    /// Whether it no longer is what `repository`'s runners in `profile` start as: other labels, or another image or
    /// other limits than the profile's. An unknown label set or image counts as the same.
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
        differ
            || self
                .image
                .as_ref()
                .is_some_and(|image| *image != profile.image.reference())
            || self.memory != profile.memory.0
            || self.cpus != u64::from(profile.cpus) * 1000
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

#[derive(Debug, Default)]
struct RepositoryState {
    queued: Vec<QueuedJob>,
    /// Whether the last poll of its queued jobs succeeded.
    listed: bool,
    held: Option<Hold>,
    /// Runners in a row that ended without taking a job.
    failures: u32,
    /// Runners in a row that ended without a job after waiting `IDLE_TIMEOUT` for one.
    idle_exits: u32,
    /// Its oldest job held for CPU that memory and a slot had room for, and since when, kept while that job stays
    /// queued: a tick held by memory or slots in between does not make it younger.
    cpu_wait: Option<CpuWait>,
    /// Whether the last plan held its next job for CPU.
    cpu_held: bool,
}

/// A job waiting for CPU, as `RepositoryState::cpu_wait` keeps it.
#[derive(Debug, Clone, Copy)]
struct CpuWait {
    /// The job's ID.
    job: u64,
    /// When a plan first held it for CPU.
    since: SystemTime,
}

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
    /// What the slots' Docker Engines may keep together, from the last check of the caches (`cache::docker_disk`);
    /// `None` until one measured Docker's disk, when each image keeps its own default.
    docker_disk: Option<u64>,
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
    /// Removed runners' build cache uploads held until `follow_jobs` asks GitHub of their jobs, by runner.
    ended_uploads: BTreeMap<String, build_cache::EndedUploads>,
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
            reseed_due: BTreeSet::new(),
            oversized: BTreeSet::new(),
            reseeded_at: BTreeMap::new(),
            docker_disk: None,
            disk: None,
            // So the caches are measured once at start.
            trim_due: true,
            trim: None,
            checked_at: SystemTime::now(),
            // Unknown: an earlier run may have left it up, so the first idle tick stops it.
            mirror_up: true,
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
            if mode != Shutdown::No
                && idle
                && self.reseeds.is_empty()
                && self.trim.is_none()
                && self.records.recordings.is_empty()
            {
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
                Some(()) = self.records.finished.recv() => {
                    self.collect_recordings().await;
                    self.publish();
                }
            }
        }
        self.close_shells().await;
        if let Err(error) = cache::mirror_down(&*self.docker, &self.names).await {
            warn!("cannot stop the mirror: {error}");
        }
        info!("stopped");
    }

    /// Runs `work` as a task of its own, answering commands and publishing after each until it is done, so a command
    /// waits for a slow GitHub no longer than for the command before it. The task goes on while a command runs: a
    /// request's timeout runs on the clock, not on polls.
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
        let (slots, reseeds) = (&self.slots, &self.reseeds);
        self.retired
            .retain(|name, _| slots.contains_key(name) || reseeds.contains_key(name));
    }
}

#[cfg(test)]
mod tests;
