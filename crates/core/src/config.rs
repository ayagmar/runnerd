//! `runnerd.toml`: the repositories to serve, the runner profiles they use, the shared caches and the host
//! budget they must fit in. Builds without the `host` feature, so clients can read a config; its only I/O is
//! reading the file and asking `systemctl` for a slice's `MemoryMax`. It never talks to Docker or GitHub.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, de};

use crate::api;

/// A parsed `runnerd.toml`. `validate` checks it against the host's budget.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The host's budget and isolation.
    pub host: Host,
    /// The caches runners share.
    pub caches: Caches,
    /// Runner profiles by name.
    pub profiles: BTreeMap<String, Profile>,
    /// The repositories to serve.
    pub repositories: Vec<Repository>,
    /// What is kept of past jobs.
    #[serde(default)]
    pub history: History,
    /// How much the records of failed jobs may keep.
    #[serde(default)]
    pub records: Records,
    /// How sizing advice reads the job history.
    #[serde(default)]
    pub sizing: Sizing,
    /// Who may use the dashboard through another address.
    #[serde(default)]
    pub web: Web,
}

/// `[host]`: where runner containers run and how they are isolated.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// The systemd slice every runner container is placed in; its `MemoryMax` is the budget.
    pub slice: String,
    /// How runner containers are kept from the host.
    pub isolation: Isolation,
    /// Must be set for `isolation = "unsafe-dind"`, so a copied config cannot turn it on silently.
    #[serde(default)]
    pub allow_unsafe: bool,
    /// How often GitHub is asked for queued jobs.
    #[serde(default = "default_poll_interval", deserialize_with = "duration")]
    pub poll_interval: Duration,
    /// What a runner must fit in before it starts. Default `caps`.
    #[serde(default)]
    pub admission: Admission,
    /// Set, a runner also starts only when its predicted CPU fits the slice's CPUs times this; absent, CPU is not
    /// admitted.
    #[serde(default)]
    pub cpu_overcommit: Option<f64>,
}

/// The largest `cpu_overcommit` the config check takes.
pub const MAX_CPU_OVERCOMMIT: f64 = 16.0;

/// `admission` under `[host]`: what a new runner must fit in the slice's `MemoryMax` beside the running ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Admission {
    /// Every runner's memory cap: `slots × memory` over all profiles, with the mirror, must fit `MemoryMax`.
    #[default]
    Caps,
    /// Every runner's predicted peak, from the job history; `slots` only bounds how many run at once, and one
    /// runner of each profile with the mirror must fit `MemoryMax`. Nothing starts while the slice is at 90% of it.
    Measured,
}

fn default_poll_interval() -> Duration {
    Duration::from_secs(15)
}

/// How runner containers are kept from the host: `isolation` under `[host]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    /// Sysbox: Docker inside an unprivileged container.
    Sysbox,
    /// Privileged Docker-in-Docker: a job can take over the host.
    UnsafeDind,
}

/// `[caches]`: what runners share and keep between jobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caches {
    /// The Docker Hub mirror.
    pub mirror: Mirror,
    /// The shared pnpm store.
    pub pnpm: CacheLimit,
    /// The shared Maven repository.
    pub maven: CacheLimit,
    /// When a profile has no job running and its slot 1 changed its layers by at least this much, slot 1's
    /// layers become the profile's new snapshot.
    #[serde(default = "default_reseed_at")]
    pub reseed_at: ByteSize,
    /// A reseed that would make a profile's snapshot larger than this starts it over from slot 1's changes alone,
    /// or from the image when those are larger too.
    #[serde(default = "default_snapshot_max")]
    pub snapshot_max: ByteSize,
}

fn default_reseed_at() -> ByteSize {
    ByteSize(2 << 30)
}

/// Above the 8.7G a profile running Flutter and Android builds needs: its Flutter SDK, NDK and Gradle caches.
fn default_snapshot_max() -> ByteSize {
    ByteSize(10 << 30)
}

fn default_mirror_max_size() -> ByteSize {
    ByteSize(10 << 30)
}

/// The Docker Hub pull-through mirror shared by all runners.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mirror {
    /// Whether runners pull through the mirror.
    pub enabled: bool,
    /// The mirror container's memory limit.
    pub memory: ByteSize,
    /// The mirror container's CPUs.
    pub cpus: u32,
    /// How long content not pulled again is kept.
    #[serde(deserialize_with = "duration")]
    pub ttl: Duration,
    /// Its storage is emptied above this size when no job runs.
    #[serde(default = "default_mirror_max_size")]
    pub max_size: ByteSize,
}

/// A size limit on a shared cache.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheLimit {
    /// The cache is emptied above this size when no job runs.
    pub max_size: ByteSize,
}

/// Where runnerd's release workflow publishes the runner images, one tag per stage and version, such as
/// `ghcr.io/ayagmar/runnerd-runner:jvm-0.0.8`.
pub const PUBLISHED_RUNNER: &str = "ghcr.io/ayagmar/runnerd-runner";

/// A runner image with its limits; every job of the profile gets one container with these limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "ProfileToml")]
pub struct Profile {
    /// The image its runners run: `image_stage` or `image`, exactly one of them.
    pub image: Image,
    /// CPUs per slot.
    pub cpus: u32,
    /// Memory per slot; the container's hard limit.
    pub memory: ByteSize,
    /// The most jobs of this profile running at once.
    pub slots: u32,
    /// Whether a failed job's runner is kept as a record, to open a shell in (`runnerctl shell`).
    pub record_failures: bool,
    /// The least `memory` auto sizing lowers it to; `None` for no bound.
    pub min_memory: Option<ByteSize>,
    /// The most `memory` auto sizing raises it to; `None` for what the slice's `MemoryMax` holds.
    pub max_memory: Option<ByteSize>,
    /// The most `slots` auto sizing raises them to; `None` leaves raising them to the owner.
    pub max_slots: Option<u32>,
}

/// The image a profile's runners run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// `image_stage`: a stage of `image/Dockerfile` built on this host as `runnerd-runner:<stage>`.
    Stage(String),
    /// `image`: a reference runnerd pulls when Docker lacks it, such as
    /// `ghcr.io/ayagmar/runnerd-runner:jvm-0.0.8`.
    Pulled(String),
}

impl Image {
    /// The reference Docker runs.
    pub fn reference(&self) -> String {
        match self {
            Image::Stage(stage) => format!("runnerd-runner:{stage}"),
            Image::Pulled(reference) => reference.clone(),
        }
    }
}

/// `[profiles.<name>]` as written, before `Profile` checks it names one image.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileToml {
    image_stage: Option<String>,
    image: Option<String>,
    cpus: u32,
    memory: ByteSize,
    slots: u32,
    #[serde(default)]
    record_failures: bool,
    min_memory: Option<ByteSize>,
    max_memory: Option<ByteSize>,
    max_slots: Option<u32>,
}

impl TryFrom<ProfileToml> for Profile {
    type Error = String;

    fn try_from(profile: ProfileToml) -> Result<Profile, String> {
        let image = match (profile.image_stage, profile.image) {
            (Some(stage), None) if !stage.is_empty() => Image::Stage(stage),
            (None, Some(reference)) if !reference.is_empty() => Image::Pulled(reference),
            (Some(_), Some(_)) => {
                return Err(
                    "set image_stage (a stage built from image/Dockerfile) or image (a reference to pull), \
                     not both"
                        .to_owned(),
                );
            }
            _ => {
                return Err(
                    "set image_stage (a stage built from image/Dockerfile) or image (a reference to pull)"
                        .to_owned(),
                );
            }
        };
        Ok(Profile {
            image,
            cpus: profile.cpus,
            memory: profile.memory,
            slots: profile.slots,
            record_failures: profile.record_failures,
            min_memory: profile.min_memory,
            max_memory: profile.max_memory,
            max_slots: profile.max_slots,
        })
    }
}

/// A repository to serve: `[[repositories]]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    /// `owner/name` on GitHub.
    pub name: String,
    /// The profile its jobs run in.
    pub profile: String,
    /// The labels its runners register with; a job runs on them when every label it asks for is one of these.
    pub labels: Vec<String>,
}

/// `[history]`: the job history database and how long it keeps what. Every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct History {
    /// The SQLite database; default `$XDG_STATE_HOME/runnerd/runnerd.db`.
    pub database: Option<PathBuf>,
    /// How long CPU and memory samples are kept; a job's totals and peaks stay. Default 7 days.
    #[serde(deserialize_with = "duration")]
    pub samples: Duration,
    /// How long a finished job's log is kept. Default 30 days.
    #[serde(deserialize_with = "duration")]
    pub logs: Duration,
    /// How long events are kept for clients resuming with `Last-Event-ID`. Default 30 days.
    #[serde(deserialize_with = "duration")]
    pub events: Duration,
    /// How long a finished job's record is kept, with its steps, totals and runner. Default 90 days.
    #[serde(deserialize_with = "duration")]
    pub jobs: Duration,
    /// The most log stored per job; later lines are dropped. Default 50M.
    pub log_max: ByteSize,
}

impl Default for History {
    fn default() -> History {
        History {
            database: None,
            samples: Duration::from_hours(7 * 24),
            logs: Duration::from_hours(30 * 24),
            events: Duration::from_hours(30 * 24),
            jobs: Duration::from_hours(90 * 24),
            log_max: ByteSize(50 << 20),
        }
    }
}

/// `[records]`: how much the records of failed jobs may keep (`record_failures` under a profile turns them on).
/// When a new record would pass a limit, the oldest records go first. Every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Records {
    /// The most disk all records may take together. Default 20G.
    pub max_total: ByteSize,
    /// The most records kept. Default 10.
    pub max_count: u32,
    /// How long a record is kept. Default 7 days.
    #[serde(deserialize_with = "duration")]
    pub max_age: Duration,
    /// The disk Docker must keep free: no record is made that would leave less. Default 10G.
    pub min_free: ByteSize,
}

impl Default for Records {
    fn default() -> Records {
        Records {
            max_total: ByteSize(20 << 30),
            max_count: 10,
            max_age: Duration::from_hours(7 * 24),
            min_free: ByteSize(10 << 30),
        }
    }
}

/// `[sizing]`: how `runnerctl sizing` turns the job history's peaks into advice, and whether the daemon applies it
/// itself. Every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sizing {
    /// The percent added to the largest sampled memory peak: a container is sampled every 2 s, so a spike between
    /// two samples goes unseen. Default 15.
    pub margin: u32,
    /// The fewest jobs a profile, or a job of a repository's workflow, needs before its peaks become advice.
    /// Default 10.
    pub min_jobs: u32,
    /// Whether the daemon writes each profile's advice into the config itself, within each profile's
    /// `min_memory`, `max_memory` and `max_slots`. Default off.
    pub auto: bool,
    /// The least time between two changes auto sizing makes to one profile. Default 24 hours.
    #[serde(deserialize_with = "duration")]
    pub interval: Duration,
    /// The most a change moves a profile's `memory`, in percent of it. Default 25.
    pub max_step: u32,
}

impl Default for Sizing {
    fn default() -> Sizing {
        Sizing {
            margin: 15,
            min_jobs: 10,
            auto: false,
            interval: Duration::from_hours(24),
            max_step: 25,
        }
    }
}

/// `[web]`: the dashboard behind a reverse proxy, such as `tailscale serve`. Read when the daemon starts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Web {
    /// The origins, besides the dashboard's own loopback address, whose pages may write: pause, reseed, save the
    /// config. Default none.
    pub origins: Vec<Origin>,
}

/// An origin as a browser sends it in `Origin`: `https://host[:port]`, or `http://` for a loopback host, in lower
/// case, without the scheme's default port, and an address written as the URL standard serializes it. Nothing after
/// the port: no path, query or user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin(String);

impl Origin {
    /// The origin as a browser sends it, such as `https://host.example.ts.net:8443`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Origin {
    type Err = String;

    fn from_str(s: &str) -> Result<Origin, String> {
        let invalid = |why: &str| {
            format!(
                "invalid origin {s:?}: {why}; write the scheme, host and port only, such as \
                 \"https://host.example.ts.net:8443\""
            )
        };
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| invalid("it has no scheme"))?;
        let scheme = scheme.to_ascii_lowercase();
        if rest.contains(['/', '?', '#']) {
            return Err(invalid("it has a path"));
        }
        if rest.contains('@') {
            return Err(invalid("it names a user"));
        }
        let (host, port) = match rest.strip_prefix('[') {
            Some(v6) => {
                let (address, after) = v6
                    .split_once(']')
                    .ok_or_else(|| invalid("its IPv6 address has no closing ]"))?;
                let port = match after {
                    "" => None,
                    _ => Some(after.strip_prefix(':').ok_or_else(|| invalid("its port"))?),
                };
                (format!("[{address}]"), port)
            }
            None => match rest.split_once(':') {
                Some((host, port)) => (host.to_owned(), Some(port)),
                None => (rest.to_owned(), None),
            },
        };
        let host = host.to_ascii_lowercase();
        let not_a_host = || invalid("its host is not a name or an address");
        // An address is kept as a browser serializes it, or it would never match the `Origin` the browser sends.
        let (host, loopback) =
            if let Some(v6) = host.strip_prefix('[').and_then(|v6| v6.strip_suffix(']')) {
                let address: Ipv6Addr = v6.parse().map_err(|_| not_a_host())?;
                (format!("[{}]", ipv6_origin(address)), address.is_loopback())
            } else {
                if host.is_empty()
                    || !host
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
                {
                    return Err(not_a_host());
                }
                // As the URL standard reads a host: one whose last label is a number is an IPv4 address.
                let name = host.strip_suffix('.').unwrap_or(&host);
                let last = name.rsplit('.').next().unwrap_or_default();
                let numeric = !last.is_empty()
                    && (last.bytes().all(|byte| byte.is_ascii_digit())
                        || last
                            .strip_prefix("0x")
                            .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit())));
                if numeric {
                    let address: Ipv4Addr = name.parse().map_err(|_| {
                        invalid("its IPv4 address is not four decimal numbers, such as 192.0.2.1")
                    })?;
                    (address.to_string(), address.is_loopback())
                } else {
                    let loopback = host == "localhost";
                    (host, loopback)
                }
            };
        let port = port
            .map(|port| {
                Some(port)
                    .filter(|port| port.bytes().all(|byte| byte.is_ascii_digit()))
                    .and_then(|port| port.parse::<u16>().ok())
                    .filter(|port| *port != 0)
                    .ok_or_else(|| invalid("its port is not a number from 1 to 65535"))
            })
            .transpose()?;
        let default_port = match scheme.as_str() {
            "https" => 443,
            "http" if loopback => 80,
            "http" => {
                return Err(invalid(
                    "http is for a loopback host only, since the session cookie would cross the network in \
                     the clear; use https",
                ));
            }
            _ => return Err(invalid("its scheme is not https")),
        };
        Ok(Origin(match port {
            Some(port) if port != default_port => format!("{scheme}://{host}:{port}"),
            _ => format!("{scheme}://{host}"),
        }))
    }
}

/// An IPv6 address as the URL standard serializes it: lower-case hex pieces, the first longest run of two or more
/// zero pieces as `::`, and no dotted IPv4 tail.
fn ipv6_origin(address: Ipv6Addr) -> String {
    let pieces = address.segments();
    let (mut zeros_at, mut zeros) = (0, 0);
    let mut at = 0;
    while at < pieces.len() {
        let run = pieces[at..].iter().take_while(|piece| **piece == 0).count();
        if run > zeros {
            (zeros_at, zeros) = (at, run);
        }
        at += run.max(1);
    }
    let hex = |pieces: &[u16]| {
        pieces
            .iter()
            .map(|piece| format!("{piece:x}"))
            .collect::<Vec<_>>()
            .join(":")
    };
    if zeros < 2 {
        return hex(&pieces);
    }
    format!(
        "{}::{}",
        hex(&pieces[..zeros_at]),
        hex(&pieces[zeros_at + zeros..])
    )
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Origin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// Why a config cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file cannot be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The config file.
        path: PathBuf,
        /// Why reading it failed.
        source: std::io::Error,
    },
    /// The file is not valid TOML or does not match the config's shape.
    #[error(transparent)]
    Parse(#[from] toml::de::Error),
    /// A repository names a profile `[profiles]` does not define.
    #[error(
        "repository {repository} uses profile {profile:?}, which is not defined under [profiles]; {}",
        defined_profiles(.defined)
    )]
    UnknownProfile {
        /// The repository's `owner/name`.
        repository: String,
        /// The profile it names.
        profile: String,
        /// The profiles `[profiles]` defines.
        defined: Vec<String>,
    },
    /// A repository has no labels, or an empty one.
    #[error("repository {repository} needs at least one non-empty label")]
    MissingLabels {
        /// The repository's `owner/name`.
        repository: String,
    },
    /// `unsafe-dind` without `allow_unsafe = true`.
    #[error(
        "isolation \"unsafe-dind\" runs runner containers privileged, so a job can take over the host; \
         set allow_unsafe = true under [host] to use it anyway"
    )]
    UnsafeNotAllowed,
    /// A limit is 0.
    #[error("{field} is 0; set it above 0")]
    ZeroLimit {
        /// The setting, such as `profiles.app.cpus`.
        field: String,
    },
    /// `cpu_overcommit` is not a number above 0 and at most `MAX_CPU_OVERCOMMIT`.
    #[error(
        "host.cpu_overcommit is {value}; set it above 0 and at most {MAX_CPU_OVERCOMMIT}, such as 1.0"
    )]
    CpuOvercommit {
        /// The value set.
        value: f64,
    },
    /// A profile's `memory` or `slots` are outside its `min_memory`, `max_memory` or `max_slots`, or its
    /// `min_memory` is above its `max_memory`.
    #[error("profiles.{profile}: {why}")]
    SizingBounds {
        /// The profile.
        profile: String,
        /// Which bound, such as `memory is above its max_memory`.
        why: &'static str,
    },
    /// The caps add up past `u64::MAX` bytes.
    #[error("slots × memory over all profiles plus the mirror is too large to add up")]
    BudgetOverflow,
    /// `admission = "measured"` with a slice that has no `MemoryMax` to admit against.
    #[error(
        "admission = \"measured\" admits runners against the slice {slice}'s MemoryMax, and it has none; install \
         a unit file for it (`runnerd doctor` prints one)"
    )]
    MeasuredWithoutMemoryMax {
        /// The slice runners run in.
        slice: String,
    },
    /// Under `admission = "measured"`, one runner of a profile and the mirror do not fit `MemoryMax`.
    #[error(
        "one runner of {profile} and the mirror take {needs}, the slice {slice}'s MemoryMax is {memory_max}; lower \
         its `memory` or raise the slice's MemoryMax"
    )]
    ProfileOverBudget {
        /// The profile.
        profile: String,
        /// Its `memory` and the mirror's.
        needs: ByteSize,
        /// The slice runners run in.
        slice: String,
        /// The slice's `MemoryMax`.
        memory_max: ByteSize,
    },
    /// The caps add up to more than the slice's `MemoryMax`.
    #[error(
        "slots × memory over all profiles plus the mirror is {total}, \
         the slice {slice}'s MemoryMax is {memory_max}; lower `slots` or `memory`, or raise the slice's \
         MemoryMax (`runnerd doctor` prints its unit)"
    )]
    OverBudget {
        /// What every runner and the mirror may use at once.
        total: ByteSize,
        /// The slice runners run in.
        slice: String,
        /// The slice's `MemoryMax`.
        memory_max: ByteSize,
    },
}

fn defined_profiles(defined: &[String]) -> String {
    if defined.is_empty() {
        "it defines none".to_owned()
    } else {
        format!("defined: {}", defined.join(", "))
    }
}

/// What a valid config should still fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// The slice has no unit file, so systemd creates it without limits.
    UnlimitedSlice {
        /// The slice runners run in.
        slice: String,
        /// What every runner and the mirror may use at once.
        total: ByteSize,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::UnlimitedSlice { slice, total } => write!(
                f,
                "the slice {slice} has no MemoryMax, so nothing holds runners to their {total} of caps; \
                 install a unit file for it"
            ),
        }
    }
}

impl Config {
    /// Reads and parses a config file.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Config::parse(&text)
    }

    /// Parses a config from TOML text.
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Parses `text` and validates it against the `MemoryMax` that `memory_max` reads for the slice it names:
    /// what `runnerd check-config` prints and `POST /v1/config/check` answers. Returns the config if it is valid.
    pub fn check(
        text: &str,
        memory_max: impl FnOnce(&str) -> std::io::Result<Option<ByteSize>>,
    ) -> (Option<Config>, api::ConfigCheck) {
        let invalid = |message: String, memory_caps: Option<ByteSize>, max: Option<ByteSize>| {
            api::ConfigCheck {
                valid: false,
                message,
                warnings: Vec::new(),
                memory_caps: memory_caps.map(|caps| caps.0),
                memory_max: max.map(|max| max.0),
                revision: None,
            }
        };
        let config = match Config::parse(text) {
            Ok(config) => config,
            Err(error) => return (None, invalid(error.to_string(), None, None)),
        };
        let caps = config.memory_caps().ok();
        let max = match memory_max(&config.host.slice) {
            Ok(max) => max,
            Err(error) => return (None, invalid(error.to_string(), caps, None)),
        };
        match config
            .validate(max)
            .and_then(|warnings| Ok((warnings, config.memory_caps()?)))
        {
            Ok((warnings, total)) => {
                let message = match (config.host.admission, max) {
                    (Admission::Measured, Some(max)) if total > max => format!(
                        "valid, memory caps sum to {total}, over the {max} MemoryMax: admission = \"measured\" \
                         starts what the predicted peaks fit"
                    ),
                    _ => format!("valid, memory caps sum to {total}"),
                };
                let check = api::ConfigCheck {
                    valid: true,
                    message,
                    warnings: warnings.iter().map(ToString::to_string).collect(),
                    memory_caps: Some(total.0),
                    memory_max: max.map(|max| max.0),
                    revision: None,
                };
                (Some(config), check)
            }
            Err(error) => (None, invalid(error.to_string(), caps, max)),
        }
    }

    /// The mirror's memory cap, 0 when it is disabled.
    pub fn mirror_memory(&self) -> u64 {
        if self.caches.mirror.enabled {
            self.caches.mirror.memory.0
        } else {
            0
        }
    }

    /// The memory every runner and the mirror may use at once.
    pub fn memory_caps(&self) -> Result<ByteSize, ConfigError> {
        let mirror = self.mirror_memory();
        self.profiles
            .values()
            .try_fold(mirror, |total, p| {
                p.memory
                    .0
                    .checked_mul(u64::from(p.slots))?
                    .checked_add(total)
            })
            .map(ByteSize)
            .ok_or(ConfigError::BudgetOverflow)
    }

    /// Refuses a profile outside the bounds auto sizing keeps it in, or whose bounds cross.
    fn check_sizing_bounds(&self) -> Result<(), ConfigError> {
        for (name, p) in &self.profiles {
            let memory = p.memory.0;
            let field = if p
                .min_memory
                .zip(p.max_memory)
                .is_some_and(|(min, max)| min.0 > max.0)
            {
                "min_memory is above its max_memory"
            } else if p.min_memory.is_some_and(|min| memory < min.0) {
                "memory is below its min_memory"
            } else if p.max_memory.is_some_and(|max| memory > max.0) {
                "memory is above its max_memory"
            } else if p.max_slots.is_some_and(|max| p.slots > max) {
                "slots are above its max_slots"
            } else {
                continue;
            };
            return Err(ConfigError::SizingBounds {
                profile: name.clone(),
                why: field,
            });
        }
        Ok(())
    }

    /// Checks the config against the slice's `MemoryMax` (`None` when the slice is unlimited).
    pub fn validate(&self, memory_max: Option<ByteSize>) -> Result<Vec<Warning>, ConfigError> {
        if self.host.isolation == Isolation::UnsafeDind && !self.host.allow_unsafe {
            return Err(ConfigError::UnsafeNotAllowed);
        }
        let zero_limits = self.profiles.iter().flat_map(|(name, p)| {
            [
                (p.memory.0 == 0, format!("profiles.{name}.memory")),
                (p.cpus == 0, format!("profiles.{name}.cpus")),
                (p.slots == 0, format!("profiles.{name}.slots")),
                (p.max_slots == Some(0), format!("profiles.{name}.max_slots")),
            ]
        });
        let mirror = &self.caches.mirror;
        let mirror_zero_limits = [
            (
                mirror.enabled && mirror.memory.0 == 0,
                "caches.mirror.memory".to_owned(),
            ),
            (
                mirror.enabled && mirror.cpus == 0,
                "caches.mirror.cpus".to_owned(),
            ),
            (
                self.host.poll_interval.is_zero(),
                "host.poll_interval".to_owned(),
            ),
            (self.records.max_count == 0, "records.max_count".to_owned()),
            (self.sizing.min_jobs == 0, "sizing.min_jobs".to_owned()),
            (self.sizing.interval.is_zero(), "sizing.interval".to_owned()),
            (self.sizing.max_step == 0, "sizing.max_step".to_owned()),
            (
                self.records.max_total.0 == 0,
                "records.max_total".to_owned(),
            ),
        ];
        if let Some((_, field)) = zero_limits
            .chain(mirror_zero_limits)
            .find(|(zero, _)| *zero)
        {
            return Err(ConfigError::ZeroLimit { field });
        }
        self.check_sizing_bounds()?;
        if let Some(value) = self
            .host
            .cpu_overcommit
            .filter(|value| !(*value > 0.0 && *value <= MAX_CPU_OVERCOMMIT))
        {
            return Err(ConfigError::CpuOvercommit { value });
        }
        for repository in &self.repositories {
            if !self.profiles.contains_key(&repository.profile) {
                return Err(ConfigError::UnknownProfile {
                    repository: repository.name.clone(),
                    profile: repository.profile.clone(),
                    defined: self.profiles.keys().cloned().collect(),
                });
            }
            if repository.labels.is_empty() || repository.labels.iter().any(|l| l.trim().is_empty())
            {
                return Err(ConfigError::MissingLabels {
                    repository: repository.name.clone(),
                });
            }
        }
        let total = self.memory_caps()?;
        if self.host.admission == Admission::Measured {
            let Some(memory_max) = memory_max else {
                return Err(ConfigError::MeasuredWithoutMemoryMax {
                    slice: self.host.slice.clone(),
                });
            };
            let mirror = self.mirror_memory();
            if let Some((profile, needs)) = self
                .profiles
                .iter()
                .map(|(name, profile)| (name, profile.memory.0.saturating_add(mirror)))
                .find(|(_, needs)| *needs > memory_max.0)
            {
                return Err(ConfigError::ProfileOverBudget {
                    profile: profile.clone(),
                    needs: ByteSize(needs),
                    slice: self.host.slice.clone(),
                    memory_max,
                });
            }
            return Ok(Vec::new());
        }
        match memory_max {
            Some(memory_max) if total > memory_max => Err(ConfigError::OverBudget {
                total,
                slice: self.host.slice.clone(),
                memory_max,
            }),
            Some(_) => Ok(Vec::new()),
            None => Ok(vec![Warning::UnlimitedSlice {
                slice: self.host.slice.clone(),
                total,
            }]),
        }
    }
}

/// Why `resize_profile` cannot change a config.
#[derive(Debug, thiserror::Error)]
pub enum ResizeError {
    /// The text is not valid TOML.
    #[error(transparent)]
    Parse(#[from] toml_edit::TomlError),
    /// `[profiles]` does not define the profile.
    #[error("the config has no profile {0:?} under [profiles]")]
    NoProfile(String),
}

/// `text` with the profile's `memory` and `slots` replaced, everything else as written, comments included: what
/// applying a sizing suggestion sends to `PUT /v1/config`, which validates it.
pub fn resize_profile(
    text: &str,
    profile: &str,
    resize: api::Resize,
) -> Result<String, ResizeError> {
    let mut document: toml_edit::DocumentMut = text.parse()?;
    let table = document
        .get_mut("profiles")
        .and_then(|profiles| profiles.get_mut(profile))
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or_else(|| ResizeError::NoProfile(profile.to_owned()))?;
    let values = [
        (
            "memory",
            toml_edit::Value::from(ByteSize(resize.memory).to_string()),
        ),
        ("slots", toml_edit::Value::from(i64::from(resize.slots))),
    ];
    for (key, mut value) in values {
        // The comment after the old value stays after the new one.
        if let Some(old) = table.get(key).and_then(toml_edit::Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        table.insert(key, toml_edit::Item::Value(value));
    }
    Ok(document.to_string())
}

/// A config text's revision: a hash of its bytes (64-bit FNV-1a, in hex), the same in every runnerd. `PUT
/// /v1/config?revision=` refuses a text sent against another revision than the daemon's.
pub fn revision(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// Reads a slice's `MemoryMax` from systemd; `None` when it is unlimited.
pub fn slice_memory_max(slice: &str) -> std::io::Result<Option<ByteSize>> {
    let output = Command::new("systemctl")
        .args(["show", slice, "--property=MemoryMax", "--value"])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "systemctl show {slice} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let value = String::from_utf8_lossy(&output.stdout);
    parse_memory_max(value.trim()).ok_or_else(|| {
        std::io::Error::other(format!(
            "systemctl show {slice}: unexpected MemoryMax {value:?}"
        ))
    })
}

/// The CPU budget, in millicores, of `millicores` of CPUs overcommitted `overcommit` times.
pub fn cpu_budget(millicores: u64, overcommit: f64) -> u64 {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a host's millicores are far below 2^52, and the config check holds the factor above 0 and at \
                  most 16"
    )]
    let budget = (millicores as f64 * overcommit.max(0.0)).round() as u64;
    budget
}

/// Reads the CPUs a slice's processes may use from systemd, in millicores: its `EffectiveCPUs` (or `AllowedCPUs`,
/// else the host's CPUs), less when its `CPUQuota` is lower.
pub fn slice_cpus(slice: &str) -> std::io::Result<u64> {
    let output = Command::new("systemctl")
        .args([
            "show",
            slice,
            "--property=EffectiveCPUs",
            "--property=AllowedCPUs",
            "--property=CPUQuotaPerSecUSec",
        ])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "systemctl show {slice} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let host = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as u64;
    parse_slice_cpus(&String::from_utf8_lossy(&output.stdout), host).ok_or_else(|| {
        std::io::Error::other(format!(
            "systemctl show {slice}: unexpected CPU properties {:?}",
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    })
}

/// The CPUs, in millicores, that `systemctl show`'s `EffectiveCPUs`, `AllowedCPUs` and `CPUQuotaPerSecUSec` lines
/// leave a slice on a host with `host` CPUs; `None` when a value does not parse.
pub fn parse_slice_cpus(properties: &str, host: u64) -> Option<u64> {
    let value = |name: &str| {
        properties.lines().find_map(|line| {
            line.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
                .map(str::trim)
        })
    };
    let set = [value("EffectiveCPUs"), value("AllowedCPUs")]
        .into_iter()
        .flatten()
        .find(|cpus| !cpus.is_empty());
    let cpus = match set {
        Some(set) => cpu_count(set)?,
        None => host,
    };
    let quota = match value("CPUQuotaPerSecUSec") {
        None | Some("" | "infinity") => u64::MAX,
        Some(quota) => quota_millicores(quota)?,
    };
    Some(cpus.saturating_mul(1000).min(quota))
}

/// How many CPUs a set names, as systemd prints it (`0-3 8-11`) or as it is written (`0-3,8,10-11`).
fn cpu_count(set: &str) -> Option<u64> {
    set.split([',', ' '])
        .filter(|range| !range.is_empty())
        .try_fold(0u64, |count, range| {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            let (first, last): (u64, u64) = (first.parse().ok()?, last.parse().ok()?);
            Some(count + last.checked_sub(first)? + 1)
        })
}

/// A `CPUQuotaPerSecUSec` as systemd prints a timespan, such as `800ms`, `2.500000s` or `1min 4s`, as the
/// millicores it allows: a second of CPU time each second is 1000.
fn quota_millicores(quota: &str) -> Option<u64> {
    // Each unit's length in milliseconds, as systemd's `format_timespan` names them.
    const UNITS: [(&str, f64); 10] = [
        ("y", 31_557_600_000.0),
        ("M", 2_629_800_000.0),
        ("w", 604_800_000.0),
        ("d", 86_400_000.0),
        ("h", 3_600_000.0),
        ("min", 60_000.0),
        ("s", 1000.0),
        ("ms", 1.0),
        ("us", 0.001),
        ("µs", 0.001),
    ];
    let millicores = quota.split_whitespace().try_fold(0.0, |total, part| {
        let at = part.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (number, unit) = part.split_at(at);
        let per_unit = UNITS
            .iter()
            .find_map(|(name, per_unit)| (*name == unit).then_some(*per_unit))?;
        Some(total + number.parse::<f64>().ok()? * per_unit)
    })?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a quota is a few thousand millicores per CPU, checked finite and not negative first"
    )]
    (!quota.trim().is_empty() && millicores.is_finite() && millicores >= 0.0)
        .then(|| millicores.round() as u64)
}

/// Parses a `MemoryMax` value as `systemctl show` prints it: bytes, or `infinity`.
pub fn parse_memory_max(value: &str) -> Option<Option<ByteSize>> {
    if value == "infinity" {
        Some(None)
    } else {
        value.parse().ok().map(|bytes| Some(ByteSize(bytes)))
    }
}

/// A size in bytes, written with a binary unit as Docker and systemd do: `512M`, `9G`, `32.5G`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

const UNITS: [(char, u64); 4] = [
    ('T', 1 << 40),
    ('G', 1 << 30),
    ('M', 1 << 20),
    ('K', 1 << 10),
];

impl FromStr for ByteSize {
    type Err = String;

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "sizes in a config are far below 2^52 bytes, and the result is checked against u64's range first"
    )]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            format!(
                "invalid size {s:?}, expected a number and a unit (K, M, G or T), such as \"9G\""
            )
        };
        let unit = s.chars().last().ok_or_else(invalid)?.to_ascii_uppercase();
        let (_, factor) = UNITS.iter().find(|(u, _)| *u == unit).ok_or_else(invalid)?;
        let number = &s[..s.len() - 1];
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return Err(invalid());
        }
        let number: f64 = number.parse().map_err(|_| invalid())?;
        let bytes = (number * *factor as f64).round();
        // `u64::MAX as f64` is 2^64, the first value out of range.
        if bytes >= u64::MAX as f64 {
            return Err(format!("size {s:?} is too large"));
        }
        Ok(ByteSize(bytes as u64))
    }
}

impl fmt::Display for ByteSize {
    #[allow(
        clippy::cast_precision_loss,
        reason = "shown with two decimals, far coarser than f64's precision"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((unit, factor)) = UNITS.iter().find(|(_, factor)| self.0 >= *factor) else {
            return write!(f, "{}B", self.0);
        };
        let number = format!("{:.2}", self.0 as f64 / *factor as f64);
        let number = number.trim_end_matches('0').trim_end_matches('.');
        write!(f, "{number}{unit}")
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// Parses a duration as Go and the registry write it, `30s`, `90m`, `168h`, or in days: `7d`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let invalid = || {
        format!(
            "invalid duration {s:?}, expected a number and s, m, h or d, such as \"168h\" or \"7d\""
        )
    };
    let (number, factor) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        _ => return Err(invalid()),
    };
    let number: u64 = number.parse().map_err(|_| invalid())?;
    number
        .checked_mul(factor)
        .map(Duration::from_secs)
        .ok_or_else(invalid)
}

/// Parses an age as the clients' `--since` and `--older-than` take it: `30m`, `12h`, `7d`, `2w`.
pub fn parse_age(text: &str) -> Result<Duration, String> {
    let error = || "expected a number and s, m, h, d or w, such as 12h".to_owned();
    let split = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(error)?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().map_err(|_| error())?;
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(error()),
    };
    Ok(Duration::from_secs(number.saturating_mul(unit)))
}

fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    parse_duration(&String::deserialize(deserializer)?).map_err(de::Error::custom)
}

#[cfg(test)]
mod tests;
