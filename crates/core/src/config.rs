//! `runnerd.toml`: the repositories to serve, the runner profiles they use, the shared caches and the host
//! budget they must fit in.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, de};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub host: Host,
    pub caches: Caches,
    pub profiles: BTreeMap<String, Profile>,
    pub repositories: Vec<Repository>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// The systemd slice every runner container is placed in; its `MemoryMax` is the budget.
    pub slice: String,
    pub isolation: Isolation,
    /// Must be set for `isolation = "unsafe-dind"`, so a copied config cannot turn it on silently.
    #[serde(default)]
    pub allow_unsafe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    Sysbox,
    /// Privileged Docker-in-Docker: a job can take over the host.
    UnsafeDind,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caches {
    pub mirror: Mirror,
    pub pnpm: CacheLimit,
    pub maven: CacheLimit,
}

/// The Docker Hub pull-through mirror shared by all runners.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mirror {
    pub enabled: bool,
    pub memory: ByteSize,
    pub cpus: u32,
    /// How long content not pulled again is kept.
    #[serde(deserialize_with = "duration")]
    pub ttl: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheLimit {
    /// The cache is emptied above this size when no job runs.
    pub max_size: ByteSize,
}

/// An image stage with its limits; every job of the profile gets one container with these limits.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub image_stage: String,
    pub cpus: u32,
    pub memory: ByteSize,
    /// The most jobs of this profile running at once.
    pub slots: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    /// `owner/name` on GitHub.
    pub name: String,
    pub profile: String,
    pub labels: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Parse(#[from] toml::de::Error),
    #[error(
        "repository {repository} uses profile {profile:?}, which is not defined under [profiles]"
    )]
    UnknownProfile { repository: String, profile: String },
    #[error("repository {repository} needs at least one non-empty label")]
    MissingLabels { repository: String },
    #[error(
        "isolation \"unsafe-dind\" runs runner containers privileged, so a job can take over the host; \
         set allow_unsafe = true under [host] to use it anyway"
    )]
    UnsafeNotAllowed,
    #[error("{field} is 0, which Docker reads as no limit; set it above 0")]
    ZeroLimit { field: String },
    #[error(
        "slots × memory over all profiles plus the mirror is {total}, \
         the slice {slice}'s MemoryMax is {memory_max}"
    )]
    OverBudget {
        total: ByteSize,
        slice: String,
        memory_max: ByteSize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// The slice has no unit file, so systemd creates it without limits.
    UnlimitedSlice { slice: String, total: ByteSize },
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
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Config::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// The memory every runner and the mirror may use at once.
    pub fn memory_caps(&self) -> ByteSize {
        let runners = self
            .profiles
            .values()
            .map(|p| p.memory.0.saturating_mul(u64::from(p.slots)))
            .fold(0u64, u64::saturating_add);
        let mirror = if self.caches.mirror.enabled {
            self.caches.mirror.memory.0
        } else {
            0
        };
        ByteSize(runners.saturating_add(mirror))
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
                });
            }
            if repository.labels.is_empty() || repository.labels.iter().any(|l| l.trim().is_empty())
            {
                return Err(ConfigError::MissingLabels {
                    repository: repository.name.clone(),
                });
            }
        }
        let total = self.memory_caps();
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
        Ok(ByteSize((number * *factor as f64).round() as u64))
    }
}

impl fmt::Display for ByteSize {
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

/// A duration as Go and the registry write it: `30s`, `90m`, `168h`.
fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(deserializer)?;
    let invalid = || {
        de::Error::custom(format!(
            "invalid duration {s:?}, expected a number and s, m or h, such as \"168h\""
        ))
    };
    let (number, factor) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        _ => return Err(invalid()),
    };
    let number: u64 = number.parse().map_err(|_| invalid())?;
    number
        .checked_mul(factor)
        .map(Duration::from_secs)
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests;
