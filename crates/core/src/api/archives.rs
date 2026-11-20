//! What clients show of the action archives runnerd keeps for its runners (ADR 0147): how many it holds, their size,
//! when it last refreshed them and what failed, each worded once here. Plain derivations of the API's types; the
//! store itself is `archives`, behind the `host` feature.

use serde::{Deserialize, Serialize};

use super::{Tone, ago};
use crate::config::ByteSize;

/// The action archives runnerd keeps, as its store last published them; in [`super::Status`] and
/// [`super::Caches`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionArchives {
    /// How many verified archives it holds.
    pub entries: u64,
    /// Their bytes.
    pub bytes: u64,
    /// When a refresh of every repository's pins last ended, in seconds since the epoch; `None` before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<u64>,
    /// What failed and has not succeeded since, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<ArchiveFailure>,
    /// Runners a restart adopted with the archives mounted, while no check of this daemon has found those good yet.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchecked: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's `skip_serializing_if` passes a reference"
)]
fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// Something the store could not do, which jobs make up for by downloading their actions themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveFailure {
    /// What failed: a served repository (`owner/name`) for [`ArchiveFailureKind::Pins`] and
    /// [`ArchiveFailureKind::Held`], a repository for [`ArchiveFailureKind::Package`], a runner for
    /// [`ArchiveFailureKind::Exposed`], the store's directory for [`ArchiveFailureKind::Store`] and
    /// [`ArchiveFailureKind::Full`], else an action (`owner/name@sha`, `owner/name/path@sha`).
    pub subject: String,
    /// What went wrong.
    pub kind: ArchiveFailureKind,
    /// When it first failed, in seconds since the epoch.
    pub since: u64,
}

/// What went wrong with an action archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveFailureKind {
    /// A served repository's pins could not be read; its last ones are kept.
    Pins,
    /// An archive could not be downloaded.
    Download,
    /// A downloaded archive failed its check, so it was not kept.
    Rejected,
    /// A kept archive no longer matched what was verified; it was moved to quarantine and is downloaded again.
    Quarantined,
    /// The store could not be opened, checked whole, or written: runners get no archives while it cannot be
    /// checked.
    Store,
    /// Not all a served repository's pins are known, so every archive it last reached is kept.
    Held,
    /// The store is at its size cap: what is missing waits for room.
    Full,
    /// The archive holds no action file at the path a pin names: not an action the runner loads.
    NotAction,
    /// The archive's action file there is not YAML runnerd reads, so what it pins is not followed.
    Unreadable,
    /// A job resolved the repository as an immutable action package, which the runner would replace with an archive
    /// of the same commit: its archives are removed and never kept.
    Package,
    /// A runner a restart adopted with the archives mounted, before the first check moved a file it may have read.
    Exposed,
    /// A kind from a newer daemon.
    #[serde(other)]
    Unknown,
}

impl ArchiveFailure {
    /// What failed, in a line: `actions/checkout@08eba0b: its archive failed its check and was not kept`.
    pub fn line(&self) -> String {
        let subject = short_subject(&self.subject);
        let what = match self.kind {
            ArchiveFailureKind::Pins => {
                "its pinned actions could not be read; the last ones read are kept"
            }
            ArchiveFailureKind::Download => "its archive could not be downloaded",
            ArchiveFailureKind::Rejected => "its archive failed its check and was not kept",
            ArchiveFailureKind::Quarantined => {
                "its kept archive changed on disk; it was moved to quarantine and is downloaded again"
            }
            ArchiveFailureKind::Store => {
                "cannot be opened, checked or written, so runners may download their actions themselves"
            }
            ArchiveFailureKind::Held => {
                "not all its pinned actions are known, so every archive it last reached is kept"
            }
            ArchiveFailureKind::Full => "is at its size cap; missing archives wait for room",
            ArchiveFailureKind::NotAction => {
                "its archive holds no action there, so it is not kept as one"
            }
            ArchiveFailureKind::Unreadable => {
                "its action file is not YAML runnerd reads, so what it pins is not kept"
            }
            ArchiveFailureKind::Package => {
                "a job resolved it as an immutable action package; its archives are removed and never kept"
            }
            ArchiveFailureKind::Exposed => {
                "kept running across a restart with the archives mounted, and the first check then moved or could \
                 not read one: its job may have read it"
            }
            ArchiveFailureKind::Unknown => "it failed",
        };
        format!("{subject}: {what}")
    }
}

/// `owner/name@sha` with the commit cut to 7 digits, as git writes one short; anything else as it is.
fn short_subject(subject: &str) -> String {
    match subject.split_once('@') {
        Some((name, sha)) if sha.len() > 7 => format!("{name}@{}", &sha[..7]),
        _ => subject.to_owned(),
    }
}

impl ActionArchives {
    /// What the store holds: `8 action archives, 12M, refreshed 3m ago`, or `no action archives yet`.
    pub fn line(&self, now: u64) -> String {
        let held = match self.entries {
            0 => "no action archives yet".to_owned(),
            1 => format!("1 action archive, {}", ByteSize(self.bytes)),
            entries => format!("{entries} action archives, {}", ByteSize(self.bytes)),
        };
        let line = match self.refreshed_at {
            Some(at) => format!("{held}, refreshed {}", ago(at, now)),
            None => held,
        };
        match self.unchecked {
            0 => line,
            1 => format!("{line}; 1 adopted runner reads them before a check"),
            unchecked => format!("{line}; {unchecked} adopted runners read them before a check"),
        }
    }

    /// What its failures mean together, in the attention list's words: `runnerd's action archives are not all kept,
    /// so jobs download those from GitHub: actions/checkout@08eba0b: … and 1 more`, or, when a runner may have read an
    /// archive a check then moved, `a runner may have read an action archive that changed: runnerd-…: …`. `None`
    /// while nothing failed.
    pub fn attention_title(&self) -> Option<String> {
        let exposed = self
            .failures
            .iter()
            .find(|failure| failure.kind == ArchiveFailureKind::Exposed);
        let first = exposed.or_else(|| self.failures.first())?;
        let more = match self.failures.len() {
            1 => String::new(),
            n => format!(" and {} more", n - 1),
        };
        Some(if exposed.is_some() {
            format!(
                "a runner may have read an action archive that changed: {}{more}",
                first.line()
            )
        } else {
            format!(
                "runnerd's action archives are not all kept, so jobs download those from GitHub: {}{more}",
                first.line()
            )
        })
    }

    /// The tone its failures take: a warning, as jobs still download what is missing.
    pub fn tone(&self) -> Tone {
        if self.failures.is_empty() {
            Tone::Neutral
        } else {
            Tone::Warn
        }
    }

    /// When its oldest failure began; `None` while nothing failed.
    pub fn failing_since(&self) -> Option<u64> {
        self.failures.iter().map(|failure| failure.since).min()
    }
}
