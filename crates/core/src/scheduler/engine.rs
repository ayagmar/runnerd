//! The scheduling loop: polls GitHub, pulls the profiles' images, applies `plan`'s decisions with GitHub and
//! Docker, removes each runner's container when its job ends, keeps the mirror up while any runner runs, reseeds
//! idle profiles and recovers runners and containers a previous daemon left. It also feeds the job history: queued
//! jobs, the job each runner took (from `observe::capture`), steps from GitHub, mirror bytes, cache sizes and slot
//! and budget changes. It owns slot state; it never serves the API (it publishes `api::Status` and takes commands
//! through `Control`) and never logs the token or a JIT config.
//!
//! This module holds `Engine`'s state, its construction and its loop; the submodules each add one concern's
//! methods: `recover` (startup), `tick` (polling and planning), `runners` (a slot's runner from start to removal),
//! `leftovers` (the containers still to read or remove once nothing occupies them), `listing` (what a repository's last listing of queued jobs showed), `ledger` (the jobs followed on GitHub, queued or closing), `claims` (what a repository's listings may show that this machine's runners took), `images` (pulling the profiles' images), `caches` (reseeds, the mirror and the shared caches), `upkeep` (what each
//! profile's measures and reseeds are doing and have due), `presence` (which repositories are listed or draining), `registrations` (what is remembered of a registration with no slot), `watches` (how each runner's container is watched: its log capture and what broken ones left), `mirror` (whether the mirror runs and at what cap), `build_cache` (each runner's side of the shared build cache and whether its job
//! may write), `history` (what the store records), `warm` (how many warm runners each repository wants), `control` (the
//! API's commands), `stop` (how far the daemon's stop has got) and `status` (what it publishes).

mod admission;
mod build_cache;
mod caches;
mod claims;
mod clock;
mod control;
mod engagement;
mod exits;
mod governor;
mod history;
mod images;
mod ledger;
mod leftovers;
mod listing;
mod mirror;
mod open_waits;
mod pool_health;
mod presence;
mod records;
mod recover;
mod registrations;
mod repository_state;
mod runner_exit;
mod runners;
mod running_reads;
mod slot;
mod start;
mod status;
mod stop;
mod tick;
mod upkeep;
mod warm;
mod watches;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use self::claims::Claims;
use self::clock::Clock;
pub use self::control::{
    Author, Command, Commands, Commit, Control, ControlError, RunningJobs, RunningSlots, Shutdown,
    Target, same_slice,
};
use self::engagement::Engagement;
use self::history::{CAPTURE_DRAIN, Found};
use self::images::Images;
use self::ledger::Ledger;
use self::leftovers::Leftovers;
use self::mirror::Mirror;
use self::presence::Repositories;
pub use self::records::RecordSelection;
use self::registrations::Registrations;
use self::repository_state::RepositoryState;
use self::slot::Slot;
use self::upkeep::Upkeep;
use self::watches::Watches;
use super::SlotRef;
use crate::admission::Key;
use crate::api;
use crate::buildcache::BuildCache;
use crate::cache::{self, Names, Trimmed};
use crate::config::Config;
use crate::docker::{self, Docker, DockerApi};
use crate::github::{Github, GithubApi};
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
    /// urgent, or the stop detaches since Docker stopped answering it. `None` for `Shutdown::No` when nothing stops.
    pub fn shutdown(&self, mode: Shutdown) -> Option<api::Stopping> {
        self.control.shutdown(mode).stopping()
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

/// Why a runner starts, as its start's log line and its record say (ADR 0092).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// For a queued job.
    Job,
    /// As one of its repository's configured warm runners, for jobs not queued yet.
    Warm,
    /// As a warm runner past those, for a job its runs are expected to queue still.
    Burst,
}

impl Purpose {
    /// How the history stores it.
    fn as_str(self) -> &'static str {
        match self {
            Purpose::Job => "job",
            Purpose::Warm => "warm",
            Purpose::Burst => "burst",
        }
    }
}

/// What else the engine does while it waits for a tick's work in `serving`, besides answering commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Meanwhile {
    /// Releases runners Docker says stopped and starts runners in their room (`turnover`).
    Turnover,
    /// Only answers commands: the work must not see a runner start or go, as a trim of the build cache.
    Commands,
}

/// What a runner, or a start in flight, takes of the budgets: what `held_by` and `reserves_cpu` count it by, so a
/// start counts as the runner it becomes.
#[derive(Debug, Clone, Copy)]
struct Footprint<'slot> {
    runner_name: &'slot str,
    /// Its container's memory cap.
    memory: u64,
    /// What it reserves of memory under measured admission.
    reserved: u64,
    /// Its container's CPU limit, in millicores.
    cpus: u64,
    /// What it reserves of the CPU budget, in millicores.
    reserved_cpu: u64,
    /// When it started, or its start was launched.
    started_at: SystemTime,
    /// Its job, as admission tells jobs apart.
    key: Option<&'slot Key>,
    /// When it took its job, if this daemon saw it do so.
    job_found_here: Option<SystemTime>,
    /// What its reservation went by.
    basis: Option<api::Basis>,
}

/// A slot's runner and its container.
#[derive(Debug, Clone)]
struct Occupant {
    repository: String,
    runner_name: String,
    runner_id: u64,
    container: String,
    started_at: SystemTime,
    /// Its container's memory cap, which a reload does not change.
    memory: u64,
    /// Its container's CPU limit in millicores, which a reload does not change.
    cpus: u64,
    /// How it was admitted, with what it reserved of the budget, kept through a restart by its record; `None` for
    /// one whose record says nothing of it, which reserves its cap.
    admission: Option<api::Admission>,
    /// The labels it registered with; `None` for one adopted from a container that does not say.
    labels: Option<Vec<String>>,
    /// The image it runs; `None` for one adopted from a container that does not say.
    image: Option<String>,
    /// The release whose container spec it runs; `None` for one adopted from a container of a release before the
    /// label.
    spec: Option<String>,
    /// Why it is there, whether it holds a job and which.
    engagement: Engagement,
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

    /// What it takes of the budgets.
    fn footprint(&self) -> Footprint<'_> {
        Footprint {
            runner_name: &self.runner_name,
            memory: self.memory,
            reserved: self.reserved(),
            cpus: self.cpus,
            reserved_cpu: self.reserved_cpu(),
            started_at: self.started_at,
            key: self.key(),
            job_found_here: self.job_found_here(),
            basis: self.admission.map(|admission| admission.basis),
        }
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

    /// Whether GitHub's last runners listing showed it busy, or GitHub said so since (`Engagement::listed_busy`).
    fn listed_busy(&self) -> bool {
        self.engagement.listed_busy()
    }

    /// The job its capture or its history row named (`Engagement::named_job`).
    fn named_job(&self) -> Option<u64> {
        self.engagement.named_job()
    }

    /// Whether `job` is the one named for it.
    fn runs(&self, job: u64) -> bool {
        self.engagement.runs(job)
    }

    /// Whether it holds a job as far as this daemon knows (`Engagement::engaged`).
    fn engaged(&self) -> bool {
        self.engagement.engaged()
    }

    /// Whether it waits for a job: not `engaged`.
    fn idle(&self) -> bool {
        !self.engaged()
    }

    /// Whether it may be running one of its pool's listed jobs that no capture named yet
    /// (`Engagement::claims_a_listed_job`).
    fn claims_a_listed_job(&self) -> bool {
        self.engagement.claims_a_listed_job()
    }

    /// Whether a detaching stop removes it (`Engagement::goes_when_detaching`).
    fn goes_when_detaching(&self) -> bool {
        self.engagement.goes_when_detaching()
    }

    /// Why this daemon started it; `None` for one a restart adopted.
    fn purpose(&self) -> Option<Purpose> {
        self.engagement.purpose()
    }

    /// When this daemon's capture found its job, if it started under this daemon (`Engagement::job_found_here`).
    fn job_found_here(&self) -> Option<SystemTime> {
        self.engagement.job_found_here()
    }

    /// Its job as admission tells jobs apart, once the history had its record.
    fn key(&self) -> Option<&Key> {
        self.engagement.key()
    }

    /// Whether admission can tell its job apart yet. A job named with no record yet has no key.
    fn has_key(&self) -> bool {
        self.key().is_some()
    }

    /// Whether it holds a job admission cannot tell apart yet, as when its log capture failed.
    fn runs_unknown_job(&self) -> bool {
        self.engaged() && !self.has_key()
    }

    fn running_job(&self) -> api::RunningJob {
        api::RunningJob {
            repository: self.repository.clone(),
            runner: self.runner_name.clone(),
            job: self.named_job(),
        }
    }
}

/// How long a stop that detaches or is forced takes at most, from its request: what Docker and GitHub have not
/// answered by then is left to the next start (ADR 0091). Well within systemd's default stop timeout of 90s, which
/// a single Docker request (120s) could otherwise outlast.
pub const STOP_DEADLINE: Duration = Duration::from_secs(30);

/// How long GitHub and Docker rest at least between the end of a tick and the start of the next, when a tick outlasts
/// the interval between their starts (ADR 0111).
const TICK_REST: Duration = Duration::from_secs(2);

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
    /// What a sample's or a host reading's age goes by.
    clock: Clock,
    memory_max: Option<u64>,
    /// The CPUs the slice may use, in millicores, read at start.
    cpus: Option<u64>,
    slots: BTreeMap<String, Vec<Slot>>,
    /// The repositories, listed by the config or draining after a reload removed them.
    repositories: Repositories,
    /// What each profile's measures and reseeds are doing and have due.
    upkeep: Upkeep,
    /// The pulled images of the config's profiles, and the removal of those no profile runs.
    images: Images,
    /// The containers still to read or remove once nothing occupies them.
    leftovers: Leftovers,
    /// Profiles a reload removed, kept while their runners or reseed still run.
    retired: BTreeMap<String, crate::config::Profile>,
    /// The slots whose last start failed preparing its caches. Not kept over a restart.
    prepare_failures: BTreeMap<SlotRef, runners::PrepareFailures>,
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
    mirror: Mirror,
    /// Until when GitHub's rate limit is used up, from the last answer that said so: no runner starts before.
    github_limited_until: Option<SystemTime>,
    /// What is known of the registrations with no slot.
    registrations: Registrations,
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
    /// How each runner's container is watched: its log capture, and what broken ones left.
    watches: Watches,
    /// Who held which address, for the mirror's log.
    leases: Leases,
    /// How far the mirror's log was counted.
    mirror_until: MirrorPosition,
    /// The jobs followed on GitHub that no runner of this machine runs now: queued, or closing once their runner
    /// went.
    ledger: Ledger,
    last_prune: Option<SystemTime>,
    /// When the running jobs were last read from GitHub, which is once every `poll_interval` (ADR 0124).
    running_reads: running_reads::RunningReads,
    /// The pace of the reads by what is left of GitHub's rate limit (ADR 0127).
    governor: governor::Governor,
    /// The records of failed jobs and the shells open in them.
    records: records::RecordState,
    /// The predictor, the high-water mark of measured admission, and what admission saw of the host.
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
    /// Released runners' registration deletes, sent in the background after their containers went (ADR 0113): each
    /// tick reaps the finished ones, and a stop waits for the rest, up to `STOP_DEADLINE`.
    deletes: Arc<Mutex<tokio::task::JoinSet<()>>>,
    /// The runner containers Docker said stopped since the last pass or tick took them (ADR 0095).
    exits: Arc<exits::Exits>,
    /// The task following Docker's events into `exits`, while the engine schedules.
    watcher: Option<exits::Watcher>,
    /// The runner containers this tick's inspect found stopped and released before Docker's event for them came: a
    /// pass the event wakes while the tick waits for GitHub starts runners into their room.
    released: BTreeSet<String>,
    /// How long the passes this tick's waits for GitHub held took. Each waits for its slowest release: a tick holds
    /// passes until they took `CAPTURE_DRAIN`, or while slots keep turning over it would take minutes to its plan.
    passes_in_tick: Duration,
    /// How many times the engine began listing GitHub's queued jobs: the listing each answer comes from, and how
    /// old a claim on a listed job is (`Claims`).
    listing: u64,
    /// By repository, the jobs its listings may show that this machine's runners took.
    claims: Claims,
    /// The second in the name of the last runner the engine named: the next one's is later, so no two runners share
    /// a name, which GitHub, Docker and the history know them by. The history keeps each before the name is used,
    /// and a restart takes it from there and the containers, so a name ahead of the clock, or a clock that stepped
    /// back, is not used again.
    named: u64,
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
            .map(|(name, profile)| {
                let mut slots = Vec::new();
                slots.resize_with(profile.slots as usize, Slot::default);
                (name.clone(), slots)
            })
            .collect();
        let repositories = Repositories::new(
            config
                .repositories
                .iter()
                .map(|repository| repository.name.clone()),
        );
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
            github_limited_until: None,
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
            clock: Clock::default(),
            isolation: isolation::backend(config.host.isolation),
            // An earlier run may have left the mirror up, so the first idle tick stops it.
            mirror: Mirror::unknown(config.mirror_memory()),
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
            upkeep: Upkeep::default(),
            images: Images::new(),
            retired: BTreeMap::new(),
            leftovers: Leftovers::default(),
            prepare_failures: BTreeMap::new(),
            ran: BTreeMap::new(),
            docker_disk,
            disk: None,
            // So the caches are measured once at start.
            trim_due: true,
            trim: None,
            checked_at: SystemTime::now(),
            github_limited_until: None,
            registrations: Registrations::default(),
            status,
            announced_profiles: Some(announced.profiles),
            control: control.clone(),
            commands,
            caches,
            store: store.clone(),
            paths,
            tracked: watch::channel(Vec::new()).0,
            found: mpsc::unbounded_channel(),
            watches: Watches::default(),
            leases: Leases::default(),
            mirror_until: MirrorPosition::default(),
            ledger: Ledger::default(),
            last_prune: None,
            running_reads: running_reads::RunningReads::default(),
            governor: governor::Governor::default(),
            measured: admission::Measured::default(),
            warm: warm::Warm::default(),
            records,
            build_cache: build_cache.clone(),
            default_branches: BTreeMap::new(),
            ended_uploads: BTreeMap::new(),
            visibility_due: Arc::default(),
            visibility_asking: None,
            deletes: Arc::default(),
            exits: Arc::default(),
            watcher: None,
            released: BTreeSet::new(),
            passes_in_tick: Duration::ZERO,
            listing: 0,
            named: 0,
            claims: Claims::default(),
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

    /// Schedules until a shutdown request is carried out, from a recovered engine. A tick is due `poll_interval`
    /// after the last one's listing started, and at least `TICK_REST` after it ended (ADR 0111). Between ticks, a
    /// runner whose container Docker says stopped is released at once, and its slot taken by a job the last poll
    /// listed (`turnover`, ADR 0095); that leaves the next tick when it was due.
    async fn schedule(&mut self) {
        self.watcher = Some(exits::Watcher::start(
            self.docker.clone(),
            self.exits.clone(),
        ));
        let mut shown = Shutdown::No;
        let mode = loop {
            let mode = self.control.mode();
            // A stop shows before it removes idle runners, which can take until `STOP_DEADLINE` on a slow GitHub or
            // Docker: until then status would show a daemon that is not stopping.
            if mode != shown {
                self.publish();
                shown = mode;
            }
            // What the next tick's due time counts from: this one's listing, or, in a detach, which lists nothing,
            // its start.
            let anchor = if mode == Shutdown::Detach {
                let began = tokio::time::Instant::now();
                self.remove_every_idle().await;
                self.collect_recordings().await;
                began
            } else {
                self.tick(mode).await
            };
            self.publish();
            // A stop asked for or changed meanwhile goes on at once, before this one counts as done: a second signal
            // while a detach removed idle runners kills the running jobs, and one Docker failed turns into a detach.
            if self.control.mode() != mode {
                continue;
            }
            if self.stopped(mode) {
                break mode;
            }
            let interval = if mode == Shutdown::No {
                self.governor.ticks_every(self.config.host.poll_interval)
            } else {
                Duration::from_secs(5)
            };
            // From the listing's start, so the tick's own length does not push the next listing out, and two listings
            // are never closer than `interval`, however long either tick took to collect before listing.
            let due =
                (anchor + interval).max(tokio::time::Instant::now() + interval.min(TICK_REST));
            self.wait_for_tick(mode, due).await;
        };
        // Nothing more is started or released: what Docker says now is the next start's.
        self.watcher = None;
        // A reseed that finished meanwhile says so; one still running is the next start's (`recover_reseeds`).
        self.collect_reseeds().await;
        self.finish_deletes().await;
        if self.control.docker_gone() {
            info!("stopped; the next start removes or adopts what Docker did not answer for");
            return;
        }
        self.close_shells().await;
        if mode == Shutdown::Detach {
            // The mirror stays up only for the busy runners left behind.
            let mirror = if self.config.caches.mirror.enabled {
                " and the mirror"
            } else {
                ""
            };
            match self.occupied().count() {
                0 => info!("stopped; no job was running"),
                1 => info!("stopped, leaving 1 busy runner{mirror} to the next start"),
                runners => {
                    info!("stopped, leaving {runners} busy runners{mirror} to the next start");
                }
            }
            return;
        }
        if let Err(error) = cache::mirror_down(&*self.docker, &self.names).await {
            warn!("cannot stop the mirror: {error}");
        }
        info!("stopped");
    }

    /// Looks at what GitHub's rate limit says before a tick and moves the pace of the reads a step if it calls for it
    /// (ADR 0127). Registrations and deletes do not ask.
    fn pace_by_limit(&mut self) {
        let spend = self.github.spend();
        if let Some(change) =
            self.governor
                .observe(tokio::time::Instant::now(), SystemTime::now(), spend)
        {
            info!("{}", change.line(self.config.host.poll_interval));
        }
    }

    /// Waits until the next tick in `mode` is `due`, or something calls for one sooner: a wake-up or a command.
    /// Runners Docker says stopped meanwhile are released by passes, finished records collected and the jobs
    /// captures name noted, all leaving `due` as it is; during a stop they call for the tick instead, which a detach does not wait for runners.
    async fn wait_for_tick(&mut self, mode: Shutdown, due: tokio::time::Instant) {
        loop {
            let exits = self.exits.clone();
            // A due tick and a command go before a pass: passes back to back would otherwise hold them while slots
            // keep turning over.
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(due) => return,
                () = self.control.wake.notified() => return,
                // A detaching stop changes nothing more: a command waiting for it fails once the engine is gone.
                Some(command) = self.commands.recv(), if mode != Shutdown::Detach => {
                    self.command(command).await;
                    self.publish();
                    return;
                }
                // Running on, a record's end changes nothing GitHub must be asked about: the next poll stays when
                // it was due. A stop returns to see whether it is done.
                Some(()) = self.records.finished.recv() => {
                    self.collect_recordings().await;
                    self.publish();
                    if mode != Shutdown::No || self.control.mode() != mode {
                        return;
                    }
                }
                // A capture named its runner's job: status says the runner runs it now, not after the next poll.
                Some(found) = self.found.1.recv() => {
                    self.note_found(&found);
                    self.publish();
                }
                () = exits.noted(), if mode != Shutdown::Detach => {
                    if mode != Shutdown::No {
                        return;
                    }
                    self.turnover().await;
                    self.publish();
                    if self.control.mode() != mode {
                        return;
                    }
                }
            }
        }
    }

    /// Whether a stop in `mode` is done: what it waits for has ended. A record being made is waited for; a reseed and
    /// a check of the caches are not, as the next start adopts the reseed and checks the caches first.
    fn stopped(&self, mode: Shutdown) -> bool {
        let idle = self.slots.values().flatten().all(Slot::is_free);
        let recorded = !self.records.is_recording();
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
    fn docker_failed(&self, error: &docker::Error) {
        if docker::is_unreachable(error) && self.control.note_docker_gone() {
            warn!("Docker does not answer during the stop; leaving the rest to the next start");
        }
    }

    /// Runs `work` as a task of its own, answering commands and publishing after each until it is done, so a command
    /// waits for a slow GitHub no longer than for the command before it. A tick follows each such command, as one
    /// follows a command answered between ticks. The task goes on while a command runs: a request's timeout runs on
    /// the clock, not on polls. With `Meanwhile::Turnover`, runners Docker says stopped are released and their slots
    /// taken meanwhile too (`turnover`, ADR 0095), so a tick's wait for GitHub does not hold them, for a
    /// while, and the jobs captures name are noted as they come.
    async fn serving<T: Send + 'static>(
        &mut self,
        work: impl Future<Output = T> + Send + 'static,
        meanwhile: Meanwhile,
    ) -> T {
        let mut task = tokio::spawn(work);
        loop {
            let exits = self.exits.clone();
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
                () = exits.noted(), if meanwhile == Meanwhile::Turnover && self.passes_in_tick < CAPTURE_DRAIN => {
                    let began = tokio::time::Instant::now();
                    self.turnover().await;
                    self.passes_in_tick += began.elapsed();
                    self.publish();
                }
                Some(found) = self.found.1.recv(), if meanwhile == Meanwhile::Turnover => {
                    self.note_found(&found);
                    self.publish();
                }
            }
        }
    }

    /// The jobs this machine's runners took, as their slots' occupants name them: computed where it is read, as a
    /// runner may take a job, or go, while the engine waits for GitHub.
    fn taken(&self) -> BTreeSet<u64> {
        self.occupied()
            .filter_map(|(_, occupant)| occupant.named_job())
            .collect()
    }

    fn occupied(&self) -> impl Iterator<Item = (SlotRef, &Occupant)> {
        self.slots.iter().flat_map(|(profile, slots)| {
            slots.iter().enumerate().filter_map(|(index, slot)| {
                slot.occupant().map(|occupant| {
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

    /// What each runner and each start in flight takes of the budgets.
    fn footprints(&self) -> impl Iterator<Item = Footprint<'_>> {
        self.slots.values().flatten().filter_map(Slot::footprint)
    }

    /// The runner in `slot`, if one is.
    fn occupant(&self, slot: &SlotRef) -> Option<&Occupant> {
        self.slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .and_then(Slot::occupant)
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
            while slots.len() > count && slots.last().is_some_and(Slot::is_free) {
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
        let (slots, upkeep) = (&self.slots, &self.upkeep);
        self.retired
            .retain(|name, _| slots.contains_key(name) || upkeep.reseed_runs(name));
    }
}

#[cfg(test)]
mod tests;
