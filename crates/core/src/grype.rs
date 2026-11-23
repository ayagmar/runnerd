//! The Grype vulnerability database runnerd keeps for its runners (ADR 0160): it reads Anchore's listing of the
//! latest database, downloads the archive it names and checks its SHA-256 against it, has Grype itself import the
//! archive and check what it imported, in a container of the pinned Grype image without a network, and publishes
//! the result as a generation no one writes again. Every runner mounts the current one read-only at [`MOUNT`], with
//! Grype's settings pointing at it and its own updates off, so a scan neither downloads nor unpacks a database on the
//! runner's disk.
//!
//! One task owns the store (`Link::start`), off the scheduling path: it checks the held database once at start,
//! reads the listing every `REFRESH` (sooner after a failure), downloads only a database built after the held one,
//! and keeps the held one through any failure. A database older than Grype accepts is not mounted, so scans fall
//! back to downloading their own. An old generation is removed only once no container's label names it.

mod listing;
mod sources;
mod store;
mod task;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bollard::models::{Mount, MountType};
use tokio::sync::watch;

pub use self::sources::{ANCHORE, Anchore, Importer};
use self::task::Keeper;
use crate::api;
use crate::config::Config;

/// Where a runner container finds the database: Grype's cache directory, holding `6/vulnerability.db`.
pub const MOUNT: &str = "/opt/runnerd/grype";

/// The label of a runner container that mounts the database, naming the generation it mounts: no generation a
/// container names is removed.
pub const LABEL: &str = "runnerd.grype-db";

/// Grype's image that imports and checks each database: v0.122.1, whose database schema is v6.1.10.
pub const IMAGE: &str = "anchore/grype:v0.122.1@sha256:661583704a253c0448ed63998b5ea2cd9d5a76ee71eedc4aed439b8ce3e93a1c";

/// How often the listing is read while all is well: Anchore builds one database a day.
const REFRESH: Duration = Duration::from_hours(6);
/// How soon after a failure the store tries again.
const RETRY: Duration = Duration::from_mins(30);
/// The largest archive downloaded: Anchore's was 207 MB on 2026-11-23.
const ARCHIVE_LIMIT: u64 = 1 << 30;

/// An archive downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// Its SHA-256 as it arrived, lowercase hex.
    pub sha256: String,
    /// Its bytes.
    pub bytes: u64,
}

/// A database Grype imported, as its own check of it reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// When Anchore built it, in seconds since the epoch.
    pub built_at: u64,
    /// Its schema, such as `v6.1.10`.
    pub schema: String,
}

/// Where the databases come from. [`Anchore`] is the real one.
pub trait Source: Send + Sync {
    /// The bytes of the latest database's listing (`latest.json`).
    fn listing(&self) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
    /// Downloads the archive the listing names `path` into the new file `to`, at most `limit` bytes.
    fn archive(
        &self,
        path: &str,
        to: &Path,
        limit: u64,
    ) -> impl Future<Output = Result<Downloaded, String>> + Send;
}

/// Grype, and the runners that read what it imported. [`Importer`] is the real one.
pub trait Grype: Send + Sync {
    /// Imports the archive `archive`, a file in `work`, into `work/db` as `grype db import` does, then checks it as
    /// `grype db status` does: its schema, its age and its digest.
    fn import(
        &self,
        work: &Path,
        archive: &str,
    ) -> impl Future<Output = Result<Imported, String>> + Send;
    /// The generations containers' [`LABEL`]s name, running or not.
    fn mounted(&self) -> impl Future<Output = Result<BTreeSet<String>, String>> + Send;
}

/// What a runner container gets of the database.
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerGrype {
    /// The generation's directory, read-only, at [`MOUNT`].
    pub mount: Mount,
    /// Grype's settings: its cache at [`MOUNT`], its updates off, its age check on.
    pub env: Vec<String>,
    /// The generation, for its container's [`LABEL`].
    pub generation: String,
}

impl RunnerGrype {
    /// What a runner gets of the generation `generation` at `dir`.
    fn of(dir: &Path, generation: &str) -> RunnerGrype {
        RunnerGrype {
            mount: Mount {
                target: Some(MOUNT.to_owned()),
                source: Some(dir.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(true),
                ..Default::default()
            },
            env: vec![
                format!("GRYPE_DB_CACHE_DIR={MOUNT}"),
                "GRYPE_DB_AUTO_UPDATE=false".to_owned(),
                "GRYPE_DB_VALIDATE_AGE=true".to_owned(),
            ],
            generation: generation.to_owned(),
        }
    }
}

/// The engine's side of the store: where it is, whether it is on, what the task last published for clients, and
/// what a runner gets. A runner reads that once as it starts. Nothing here survives a restart: the new task checks the
/// held database before it publishes it.
#[derive(Debug)]
pub struct Link {
    /// The store's directory; `None` when the daemon could not name one.
    directory: Option<PathBuf>,
    enabled: watch::Sender<bool>,
    status: watch::Sender<Option<api::GrypeDatabase>>,
    /// What a runner gets while a checked database young enough is held; `None` otherwise.
    ready: watch::Sender<Option<RunnerGrype>>,
}

impl Link {
    /// The link to a store at `directory`, going by `config`.
    pub fn new(directory: Option<PathBuf>, config: &Config) -> Link {
        Link {
            directory,
            enabled: watch::Sender::new(config.caches.grype.enabled),
            status: watch::Sender::new(None),
            ready: watch::Sender::new(None),
        }
    }

    /// Starts the task that owns the store, unless there is no directory. Dropping what it returns stops it.
    pub fn start<S: Source + 'static, G: Grype + 'static>(
        &self,
        source: Arc<S>,
        grype: Arc<G>,
    ) -> Option<Running> {
        let directory = self.directory.clone()?;
        let task = tokio::spawn(run(
            source,
            grype,
            Keeper::new(directory),
            self.enabled.subscribe(),
            self.status.clone(),
            self.ready.clone(),
        ));
        Some(Running(Some(task)))
    }

    /// Hands the task `config`'s setting, after a reload. Turned off, runners get no database from then on.
    pub fn reload(&self, config: &Config) {
        let enabled = config.caches.grype.enabled;
        if !enabled {
            self.ready.send_replace(None);
        }
        self.enabled.send_if_modified(|on| {
            let changed = *on != enabled;
            *on = enabled;
            changed
        });
    }

    /// What clients show of the store: `None` while it is off or before its task first published.
    pub fn status(&self) -> Option<api::GrypeDatabase> {
        if !*self.enabled.borrow() {
            return None;
        }
        self.status.borrow().clone()
    }

    /// What a runner container mounts and its environment gets, read once as it starts.
    pub fn runner(&self) -> Option<RunnerGrype> {
        if !*self.enabled.borrow() {
            return None;
        }
        self.ready.borrow().clone()
    }

    /// Publishes `generation` at `dir` to runners, as the task would after checking it.
    #[cfg(test)]
    pub(crate) fn serve(&self, dir: &Path, generation: &str) {
        self.ready
            .send_replace(Some(RunnerGrype::of(dir, generation)));
    }
}

/// The running task; dropping it stops the task. A download or import stopped midway leaves a work directory in
/// `staging/`, which the next start removes, and an import container, which the next import removes first.
#[derive(Debug)]
pub struct Running(Option<tokio::task::JoinHandle<()>>);

impl Running {
    /// Stops the task and waits until it has.
    pub async fn stop(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Owns the store: while on, opens and checks it, publishes the held database to runners, then updates it from the
/// listing and publishes again, every `REFRESH`, `RETRY` after a failure, before the held one grows too old, and
/// whenever the setting changes.
async fn run<S: Source + 'static, G: Grype + 'static>(
    source: Arc<S>,
    grype: Arc<G>,
    mut keeper: Keeper,
    mut enabled: watch::Receiver<bool>,
    status: watch::Sender<Option<api::GrypeDatabase>>,
    ready: watch::Sender<Option<RunnerGrype>>,
) {
    loop {
        let on = *enabled.borrow_and_update();
        let wait = if on {
            keeper.prepare(unix_now()).await;
            ready.send_replace(keeper.runner(unix_now()));
            status.send_replace(Some(keeper.status(unix_now())));
            let wait = keeper.update(&*source, &*grype, unix_now()).await;
            // The setting may have turned off meanwhile: `Link::reload` withdrew the view, which stays withdrawn.
            if *enabled.borrow() {
                ready.send_replace(keeper.runner(unix_now()));
            }
            status.send_replace(Some(keeper.status(unix_now())));
            keeper
                .until_stale(unix_now())
                .map_or(wait, |stale| wait.min(stale))
        } else {
            ready.send_replace(None);
            REFRESH
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            changed = enabled.changed() => if changed.is_err() { return },
        }
    }
}
