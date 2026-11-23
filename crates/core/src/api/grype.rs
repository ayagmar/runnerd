//! What clients show of the Grype vulnerability database runnerd keeps for its runners (ADR 0160): which database
//! it holds, whether new runners mount it, when Anchore's listing was last read and what failed, each worded once
//! here. Plain derivations of the API's types; the store itself is `grype`, behind the `host` feature.

use serde::{Deserialize, Serialize};

use super::{Tone, ago};
use crate::config::ByteSize;

/// How old a database Grype accepts by default (`db.max-allowed-built-age`, 120h): runnerd mounts none older.
pub const GRYPE_MAX_AGE_SECS: u64 = 5 * 86_400;

/// The Grype database runnerd keeps, as its store last published it; in [`super::Status`] and [`super::Caches`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrypeDatabase {
    /// The database it holds, as Grype checked it; `None` before the first was imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<HeldGrypeDatabase>,
    /// Whether a runner that starts now mounts it.
    #[serde(default)]
    pub mounted: bool,
    /// When Anchore's listing was last read, in seconds since the epoch; `None` before the first read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    /// What failed and has not succeeded since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<GrypeFailure>,
}

/// A database runnerd holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldGrypeDatabase {
    /// When Anchore built it, in seconds since the epoch.
    pub built_at: u64,
    /// Its schema, as Grype writes it: `v6.1.10`.
    pub schema: String,
    /// The bytes of its directory.
    pub bytes: u64,
}

/// Something the store could not do; the database it held, if any, stays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrypeFailure {
    /// What went wrong.
    pub kind: GrypeFailureKind,
    /// What the failing step said, in a line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// When it first failed, in seconds since the epoch.
    pub since: u64,
}

/// What went wrong with the Grype database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrypeFailureKind {
    /// Anchore's listing could not be read, or is not one runnerd takes.
    Listing,
    /// The archive the listing names could not be downloaded.
    Download,
    /// The downloaded archive does not have the listing's checksum.
    Checksum,
    /// Grype could not import the archive, or its check of what it imported failed.
    Import,
    /// The store's directory could not be opened, checked or written.
    Store,
    /// The database held is older than Grype accepts, so runners do not mount it.
    Stale,
    /// A kind from a newer daemon.
    #[serde(other)]
    Unknown,
}

impl GrypeFailure {
    /// What failed, in a line: `the archive could not be downloaded (timed out); runners keep the database held`.
    pub fn line(&self) -> String {
        let what = match self.kind {
            GrypeFailureKind::Listing => "Anchore's listing of Grype databases could not be read",
            GrypeFailureKind::Download => "the database's archive could not be downloaded",
            GrypeFailureKind::Checksum => {
                "the downloaded archive does not match the listing's checksum"
            }
            GrypeFailureKind::Import => {
                "Grype could not import the archive or check what it imported"
            }
            GrypeFailureKind::Store => "its directory cannot be opened, checked or written",
            GrypeFailureKind::Stale => {
                "the database held is older than the 5 days Grype accepts, so runners do not mount it"
            }
            GrypeFailureKind::Unknown => "it failed",
        };
        if self.detail.is_empty() {
            what.to_owned()
        } else {
            format!("{what} ({})", self.detail)
        }
    }
}

impl GrypeDatabase {
    /// What the store holds: `Grype database v6.1.10 built 6h 12m ago, 1.2G, mounted in runners, checked 3m ago`, or
    /// `no Grype database yet`.
    pub fn line(&self, now: u64) -> String {
        let held = match &self.held {
            None => "no Grype database yet".to_owned(),
            Some(held) => format!(
                "Grype database {} built {}, {}, {}",
                held.schema,
                ago(held.built_at, now),
                ByteSize(held.bytes),
                if self.mounted {
                    "mounted in runners"
                } else {
                    "not mounted in runners"
                }
            ),
        };
        match self.checked_at {
            Some(at) => format!("{held}, checked {}", ago(at, now)),
            None => held,
        }
    }

    /// Its failure in the attention list's words: `runnerd's Grype database is not current, so scans may download
    /// their own: …`. `None` while nothing failed.
    pub fn attention_title(&self) -> Option<String> {
        let failure = self.failure.as_ref()?;
        Some(format!(
            "runnerd's Grype database is not current, so scans may download their own: {}",
            failure.line()
        ))
    }

    /// The tone its failure takes: a warning, as a scan without it downloads its own.
    pub fn tone(&self) -> Tone {
        if self.failure.is_some() {
            Tone::Warn
        } else {
            Tone::Neutral
        }
    }
}
