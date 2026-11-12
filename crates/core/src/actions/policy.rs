//! What the config lets the owner do with jobs (ADR 0134): `[github] job_actions` and the repositories, one snapshot
//! replaced whole, with a revision that moves whenever either changes. A request is authorized against one snapshot,
//! never against two reads, and a confirmation binds the revision it was issued under.
//!
//! It must not hold its lock across anything but the swap, and it survives nothing: the engine sets it at start and
//! at each reload.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::api;
use crate::sync::lock;

/// The config of one moment, as the job actions read it. Never changed: a reload makes a new one.
#[derive(Debug, PartialEq, Eq)]
pub struct Policy {
    revision: u64,
    setting: api::JobActions,
    repositories: BTreeSet<String>,
}

/// Why a policy does not let the owner act on a repository's jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// `[github] job_actions` is off.
    Off,
    /// The config does not have the repository.
    NotConfigured,
}

impl Policy {
    /// Which snapshot this is; a later one with other content has a higher revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether this config lets the owner cancel and re-run `repository`'s jobs.
    pub fn allows(&self, repository: &str) -> Result<(), Denied> {
        if self.setting != api::JobActions::On {
            return Err(Denied::Off);
        }
        if !self.repositories.contains(repository) {
            return Err(Denied::NotConfigured);
        }
        Ok(())
    }
}

/// The current [`Policy`]. Only its methods here write it.
#[derive(Debug)]
pub struct Policies {
    current: Mutex<Arc<Policy>>,
}

impl Policies {
    /// The first snapshot: `setting` for `repositories`.
    pub fn new(setting: api::JobActions, repositories: BTreeSet<String>) -> Policies {
        Policies {
            current: Mutex::new(Arc::new(Policy {
                revision: 1,
                setting,
                repositories,
            })),
        }
    }

    /// The snapshot of this moment.
    pub fn now(&self) -> Arc<Policy> {
        lock(&self.current).clone()
    }

    /// Replaces the snapshot whole; its revision moves only when `setting` or `repositories` differ from it.
    pub fn replace(&self, setting: api::JobActions, repositories: BTreeSet<String>) {
        let mut current = lock(&self.current);
        if current.setting == setting && current.repositories == repositories {
            return;
        }
        *current = Arc::new(Policy {
            revision: current.revision + 1,
            setting,
            repositories,
        });
    }
}
