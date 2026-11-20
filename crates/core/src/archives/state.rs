//! What the archives task knows besides the store: each served repository's pins as last read, at the commit they
//! were read at, and what failed and has not succeeded since. Only its methods write it. It lives in memory: a
//! restart reads every repository's pins again and forgets the failures; a reload keeps it, and the next refresh
//! drops what the config no longer serves.

use std::collections::{BTreeMap, BTreeSet};

use super::store::Key;
use crate::api;

/// An action a refresh wants kept: its archive, and the directory of the action in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Wanted {
    pub(crate) key: Key,
    /// `""` for the repository's root.
    pub(crate) path: String,
}

/// A repository's pins, as read at one commit of its default branch.
#[derive(Debug, Clone)]
struct Pins {
    head: String,
    wanted: BTreeSet<Wanted>,
}

/// The archives task's memory.
#[derive(Debug, Default)]
pub(crate) struct Refresher {
    /// By served repository, its last good pins.
    pins: BTreeMap<String, Pins>,
    /// By subject (`owner/name` or `owner/name@sha`), what failed.
    failures: BTreeMap<String, api::ArchiveFailure>,
    /// When the last refresh ended, in seconds since the epoch.
    refreshed_at: Option<u64>,
}

impl Refresher {
    /// The commit `repository`'s pins were last read at.
    pub(crate) fn head(&self, repository: &str) -> Option<&str> {
        self.pins.get(repository).map(|pins| pins.head.as_str())
    }

    /// Keeps `wanted` as `repository`'s pins at `head`, replacing what was read before.
    pub(crate) fn read(&mut self, repository: &str, head: String, wanted: BTreeSet<Wanted>) {
        self.pins
            .insert(repository.to_owned(), Pins { head, wanted });
    }

    /// Every served repository's pins, as last read.
    pub(crate) fn wanted(&self) -> BTreeSet<Wanted> {
        self.pins
            .values()
            .flat_map(|pins| pins.wanted.iter().cloned())
            .collect()
    }

    /// Notes that `subject` failed as `kind` at `now`; one failing still keeps when it began.
    pub(crate) fn fail(&mut self, subject: &str, kind: api::ArchiveFailureKind, now: u64) {
        let failure =
            self.failures
                .entry(subject.to_owned())
                .or_insert_with(|| api::ArchiveFailure {
                    subject: subject.to_owned(),
                    kind,
                    since: now,
                });
        if failure.kind != kind {
            *failure = api::ArchiveFailure {
                subject: subject.to_owned(),
                kind,
                since: now,
            };
        }
    }

    /// Notes that `subject` succeeded: its failure, if any, is over.
    pub(crate) fn succeed(&mut self, subject: &str) {
        self.failures.remove(subject);
    }

    /// Ends a refresh at `now`: forgets the pins of repositories no longer `served`, and the failures of what is
    /// neither served nor `wanted` any more.
    pub(crate) fn finish(&mut self, served: &[String], wanted: &BTreeSet<Key>, now: u64) {
        self.pins
            .retain(|repository, _| served.contains(repository));
        let labels: BTreeSet<String> = wanted.iter().map(Key::label).collect();
        self.failures.retain(|subject, failure| match failure.kind {
            api::ArchiveFailureKind::Pins => served.contains(subject),
            api::ArchiveFailureKind::Store => true,
            _ => labels.contains(subject),
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
        }
    }
}
