//! What the config lets the owner do with jobs (ADR 0134): `[github] job_actions`, the repositories and which of them
//! have their runner faults re-run (ADR 0153), one snapshot replaced whole, with a revision that moves whenever any
//! changes. A request is authorized against one snapshot,
//! never against two reads, and a confirmation binds the revision it was issued under.
//!
//! It must not hold its lock across anything but the swap, and it survives nothing: the engine sets it at start and
//! at each reload.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::api;
use crate::sync::lock;

/// The config of one moment, as the job actions read it. Never changed: a reload makes a new one.
#[derive(Debug, PartialEq, Eq)]
pub struct Policy {
    revision: u64,
    setting: api::JobActions,
    repositories: BTreeSet<String>,
    reruns: BTreeSet<String>,
}

/// The config a [`Policy`] is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    /// `[github] job_actions`.
    pub setting: api::JobActions,
    /// The configured repositories.
    pub repositories: BTreeSet<String>,
    /// Those whose `rerun_runner_faults` is on.
    pub reruns: BTreeSet<String>,
}

impl Allowed {
    /// What `config` allows.
    pub fn of(config: &crate::config::Config) -> Allowed {
        let repositories = config.repositories.iter();
        Allowed {
            setting: api::JobActions::configured(config.github.job_actions),
            repositories: repositories
                .clone()
                .map(|repository| repository.name.clone())
                .collect(),
            reruns: repositories
                .filter(|repository| repository.rerun_runner_faults)
                .map(|repository| repository.name.clone())
                .collect(),
        }
    }
}

/// Why a policy does not let the owner act on a repository's jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// `[github] job_actions` is off.
    Off,
    /// The config does not have the repository.
    NotConfigured,
    /// The repository's `rerun_runner_faults` is off.
    RerunsOff,
}

impl Policy {
    fn of(revision: u64, allowed: Allowed) -> Policy {
        Policy {
            revision,
            setting: allowed.setting,
            repositories: allowed.repositories,
            reruns: allowed.reruns,
        }
    }

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

    /// Whether this config lets runnerd re-run `repository`'s runs its runner failed (ADR 0153): job actions on, the
    /// repository configured and its `rerun_runner_faults` on.
    pub fn reruns(&self, repository: &str) -> Result<(), Denied> {
        self.allows(repository)?;
        if !self.reruns.contains(repository) {
            return Err(Denied::RerunsOff);
        }
        Ok(())
    }
}

/// The current [`Policy`]. Only its methods here write it.
#[derive(Debug)]
pub struct Policies {
    current: Mutex<Arc<Policy>>,
    /// The current revision, for what waits for a replacement.
    revisions: watch::Sender<u64>,
}

impl Policies {
    /// The first snapshot: what `allowed` says.
    pub fn new(allowed: Allowed) -> Policies {
        Policies {
            current: Mutex::new(Arc::new(Policy::of(1, allowed))),
            revisions: watch::Sender::new(1),
        }
    }

    /// The snapshot of this moment.
    pub fn now(&self) -> Arc<Policy> {
        lock(&self.current).clone()
    }

    /// Replaces the snapshot whole; its revision moves only when `allowed` differs from it.
    pub fn replace(&self, allowed: Allowed) {
        let mut current = lock(&self.current);
        if current.setting == allowed.setting
            && current.repositories == allowed.repositories
            && current.reruns == allowed.reruns
        {
            return;
        }
        *current = Arc::new(Policy::of(current.revision + 1, allowed));
        // Under the lock, so a waiter sees revisions in order.
        self.revisions.send_replace(current.revision);
    }

    /// Resolves once the snapshot is another than revision `revision`.
    pub async fn moved(&self, revision: u64) {
        let mut revisions = self.revisions.subscribe();
        // `self` holds the sender, so the wait ends only at a replacement.
        let _ = revisions.wait_for(|now| *now != revision).await;
    }
}
