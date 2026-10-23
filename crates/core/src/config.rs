//! `runnerd.toml`: the repositories to serve, the runner profiles they use, the shared caches and the host
//! budget they must fit in. Builds without the `host` feature, so clients can read a config; its only I/O is
//! reading the file and asking `systemctl` for a slice's `MemoryMax`. It never talks to Docker or GitHub.

use std::collections::BTreeMap;
use std::fmt;
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
}

fn default_reseed_at() -> ByteSize {
    ByteSize(2 << 30)
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
}

/// A size limit on a shared cache.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheLimit {
    /// The cache is emptied above this size when no job runs.
    pub max_size: ByteSize,
}

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
}

/// The image a profile's runners run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// `image_stage`: a stage of `image/Dockerfile` built on this host as `runnerd-runner:<stage>`.
    Stage(String),
    /// `image`: a reference runnerd pulls when Docker lacks it, such as
    /// `ghcr.io/ayagmar/runnerd-runner:jvm-0.0.6`.
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

/// `[sizing]`: how `runnerctl sizing` turns the job history's peaks into advice. Every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sizing {
    /// The percent added to the largest sampled memory peak: a container is sampled every 2 s, so a spike between
    /// two samples goes unseen. Default 15.
    pub margin: u32,
    /// The fewest jobs a profile, or a job of a repository's workflow, needs before its peaks become advice.
    /// Default 10.
    pub min_jobs: u32,
}

impl Default for Sizing {
    fn default() -> Sizing {
        Sizing {
            margin: 15,
            min_jobs: 10,
        }
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
    /// The caps add up past `u64::MAX` bytes.
    #[error("slots × memory over all profiles plus the mirror is too large to add up")]
    BudgetOverflow,
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
                let check = api::ConfigCheck {
                    valid: true,
                    message: format!("valid, memory caps sum to {total}"),
                    warnings: warnings.iter().map(ToString::to_string).collect(),
                    memory_caps: Some(total.0),
                    memory_max: max.map(|max| max.0),
                };
                (Some(config), check)
            }
            Err(error) => (None, invalid(error.to_string(), caps, max)),
        }
    }

    /// The memory every runner and the mirror may use at once.
    pub fn memory_caps(&self) -> Result<ByteSize, ConfigError> {
        let mirror = if self.caches.mirror.enabled {
            self.caches.mirror.memory.0
        } else {
            0
        };
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

fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    parse_duration(&String::deserialize(deserializer)?).map_err(de::Error::custom)
}

#[cfg(test)]
mod tests;
