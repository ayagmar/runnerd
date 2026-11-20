//! What the archives task knows besides the store: what failed and has not succeeded since, and when the last
//! refresh ended. Only its methods write it. It lives in memory: a restart forgets the failures; a reload keeps it,
//! and the next refresh drops what the config no longer serves. The pins live in the store's index.

use std::collections::{BTreeMap, BTreeSet};

use super::store::Key;
use crate::api;

/// An action a refresh wants kept: its archive, and the directory of the action in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub(crate) struct Wanted {
    #[serde(flatten)]
    pub(crate) key: Key,
    /// `""` for the repository's root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) path: String,
}

impl Wanted {
    /// How a failure names it: `owner/repo@sha` for an action at the root, `owner/repo/path@sha` for one deeper.
    pub(crate) fn subject(&self) -> String {
        if self.path.is_empty() {
            self.key.label()
        } else {
            format!("{}/{}@{}", self.key.repository, self.path, self.key.sha)
        }
    }
}

/// The archives task's memory.
#[derive(Debug, Default)]
pub(crate) struct Refresher {
    /// By subject (`owner/name`, `owner/name@sha`, a runner's name or the store's directory) and kind, what failed.
    failures: BTreeMap<(String, api::ArchiveFailureKind), api::ArchiveFailure>,
    /// When the last refresh ended, in seconds since the epoch.
    refreshed_at: Option<u64>,
}

impl Refresher {
    /// Notes that `subject` failed as `kind` at `now`; one failing still keeps when it began.
    pub(crate) fn fail(&mut self, subject: &str, kind: api::ArchiveFailureKind, now: u64) {
        self.failures
            .entry((subject.to_owned(), kind))
            .or_insert_with(|| api::ArchiveFailure {
                subject: subject.to_owned(),
                kind,
                since: now,
            });
    }

    /// Notes that what `subject` did succeeded: its failures are over, but for what stands until it changes (a
    /// retention held, a runner exposed, a package).
    pub(crate) fn succeed(&mut self, subject: &str) {
        self.failures.retain(|(failed, kind), _| {
            failed != subject
                || matches!(
                    kind,
                    api::ArchiveFailureKind::Held
                        | api::ArchiveFailureKind::Exposed
                        | api::ArchiveFailureKind::Package
                )
        });
    }

    /// Notes that `subject`'s failure of `kind` is over.
    pub(crate) fn succeed_kind(&mut self, subject: &str, kind: api::ArchiveFailureKind) {
        self.failures.remove(&(subject.to_owned(), kind));
    }

    /// Ends a refresh at `now`: forgets the failures of repositories no longer `served`, and of actions not among
    /// `subjects`, what the refresh reached. The store's (`store`), a runner's and a package's stay.
    pub(crate) fn finish(
        &mut self,
        served: &[String],
        subjects: &BTreeSet<String>,
        store: &str,
        now: u64,
    ) {
        self.failures.retain(|(subject, kind), _| match kind {
            api::ArchiveFailureKind::Pins | api::ArchiveFailureKind::Held => {
                served.contains(subject)
            }
            api::ArchiveFailureKind::Store | api::ArchiveFailureKind::Full => subject == store,
            api::ArchiveFailureKind::Exposed | api::ArchiveFailureKind::Package => true,
            _ => subjects.contains(subject),
        });
        self.refreshed_at = Some(now);
    }

    /// What clients show, with the store's `entries` and `bytes`.
    pub(crate) fn status(&self, entries: u64, bytes: u64) -> api::ActionArchives {
        let mut failures: Vec<api::ArchiveFailure> = self.failures.values().cloned().collect();
        failures.sort_by(|a, b| {
            a.since
                .cmp(&b.since)
                .then_with(|| a.subject.cmp(&b.subject))
        });
        api::ActionArchives {
            entries,
            bytes,
            refreshed_at: self.refreshed_at,
            failures,
            unchecked: 0,
        }
    }
}
