//! The action archives runnerd keeps for its runners (ADR 0147): it reads which actions each served repository's
//! default branch pins by commit, downloads each public one's archive from codeload without a credential, verifies it,
//! and keeps it where every runner reads it, read-only, as `ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE`, so a job copies it
//! instead of downloading it. An archive it does not hold, a job downloads itself, as without it.
//!
//! One task owns the store (`run`), off the scheduling path: it refreshes at start and every `REFRESH`, reading a
//! repository's tree and files only when its default branch moved, and publishes the view runners mount only after a
//! check found every kept file good. It removes an archive only after a refresh that knew every pin whole. It never
//! fetches a private action, never takes what to fetch from a job's log, and never sets
//! `ACTIONS_RUNNER_SYMLINK_CACHED_ACTIONS`. The token stays in `github::Github` and is never sent for an archive.

mod pins;
mod state;
mod store;
mod verify;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bollard::models::{Mount, MountType};
use futures_util::StreamExt;
use tokio::sync::watch;
use tracing::{info, warn};

use self::pins::{MAX_DEPTH, Pin};
use self::state::{Refresher, Wanted};
use self::store::{ArchiveStore, Key, Staged};
use crate::api;
use crate::config::Config;
use crate::github::Error;
use crate::sync::lock;

/// Where a runner container finds the archives.
pub const MOUNT: &str = "/opt/runnerd/actions";

/// The runner's setting that points it at the archives: `ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE`.
pub const ENV: &str = "ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE";

/// How often the store refreshes: each served repository is asked for its default branch and that branch's head this
/// often, two requests each while it is unchanged, and its tree and files only when the head moved.
const REFRESH: Duration = Duration::from_secs(600);

/// What one refresh may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The largest archive downloaded: far above any action's.
    pub(crate) archive: u64,
    /// The most bytes one refresh downloads, failed and refused downloads included; what is left waits for the next.
    pub(crate) bytes: u64,
    /// The most requests one refresh asks of GitHub's API; what is left waits for the next.
    pub(crate) requests: u32,
}

/// The limits runnerd refreshes by.
const LIMITS: Limits = Limits {
    archive: 256 << 20,
    bytes: 2 << 30,
    requests: 1000,
};

/// What a refresh leaves of GitHub's rate limit: below it, it asks nothing more, as runners' registrations and the
/// scheduler's reads come first.
const RESERVE: u64 = 1000;

/// How many archives download at once.
const DOWNLOADS: usize = 3;

/// How many times an archive's download is tried in one refresh, and the wait before each retry.
const ATTEMPTS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(5),
    Duration::from_secs(20),
];

/// The most files of a repository's tree read for pins, and the largest read.
const PIN_FILES: usize = 300;
const PIN_FILE_BYTES: u64 = 1 << 20;

/// A repository as the archives need it from `GET /repos/{owner}/{repo}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRepository {
    /// `owner/repo` as GitHub names it, case and all.
    pub full_name: String,
    /// Whether it is public: only a public action's archive is kept.
    pub public: bool,
    /// Its default branch.
    pub default_branch: String,
}

/// A repository's tree at a commit, as `GET /repos/{owner}/{repo}/git/trees/{sha}?recursive=1` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    /// Whether GitHub cut it short: its files are then not all there.
    pub truncated: bool,
    /// Its files.
    pub files: Vec<TreeFile>,
}

/// A file of a [`Tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    /// Its path from the repository's root.
    pub path: String,
    /// Its blob's SHA.
    pub blob: String,
    /// Its bytes.
    pub size: u64,
}

/// What the archives read from GitHub. `github::Github` is the real one; the token never leaves it.
pub trait Source: Send + Sync {
    /// A repository's canonical name, visibility and default branch.
    fn action_repository(
        &self,
        repository: &str,
    ) -> impl Future<Output = Result<ActionRepository, Error>> + Send;
    /// The commit `branch` of `repository` is at.
    fn branch_head(
        &self,
        repository: &str,
        branch: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send;
    /// The repository's tree at `commit`, every file of it.
    fn tree(
        &self,
        repository: &str,
        commit: &str,
    ) -> impl Future<Output = Result<Tree, Error>> + Send;
    /// A file's bytes by its blob's SHA, at most `limit` of them.
    fn blob(
        &self,
        repository: &str,
        blob: &str,
        limit: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send;
    /// Downloads the archive of `repository` at `commit` anonymously (codeload.github.com's `/{owner}/{repo}/tar.gz/
    /// {sha}`) into the new file `to`, at most `limit` bytes, and returns its size. Never with a token: a repository
    /// that is not public is not there.
    fn tarball(
        &self,
        repository: &str,
        commit: &str,
        to: &Path,
        limit: u64,
    ) -> impl Future<Output = Result<u64, Error>> + Send;
    /// The requests left of GitHub's rate limit before it resets, as last read; `None` when no answer said.
    fn spare_requests(&self) -> Option<u64>;
}

/// What the store goes by, from the config: replaced whole at each reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `[caches.actions] enabled`.
    pub enabled: bool,
    /// `[caches.actions] keep`.
    pub keep: Duration,
    /// The served repositories, each once.
    pub repositories: Vec<String>,
}

impl Settings {
    /// The settings of `config`.
    pub fn of(config: &Config) -> Settings {
        Settings {
            enabled: config.caches.actions.enabled,
            keep: config.caches.actions.keep,
            repositories: config.repository_names(),
        }
    }
}

/// The engine's side of the store: where it is, the settings it hands the task, and what the task last published:
/// what clients show, and what runners get. The task publishes the runners' view only after the store opened and a
/// check found every file under `archives/` good, and withdraws it when a check does not or the archives go off; a
/// runner reads it once as it starts. Nothing survives a restart: a new task checks before it publishes.
#[derive(Debug)]
pub struct Link {
    /// The store's directory; `None` when the daemon could not name one.
    directory: Option<PathBuf>,
    settings: watch::Sender<Settings>,
    status: watch::Sender<Option<api::ActionArchives>>,
    /// What a runner gets while the archives are checked good; `None` otherwise.
    ready: watch::Sender<Option<RunnerArchives>>,
}

impl Link {
    /// The link to a store at `directory`, going by `config`.
    pub fn new(directory: Option<PathBuf>, config: &Config) -> Link {
        Link {
            directory,
            settings: watch::Sender::new(Settings::of(config)),
            status: watch::Sender::new(None),
            ready: watch::Sender::new(None),
        }
    }

    /// Starts the task that owns the store, unless there is no directory. Dropping what it returns stops it.
    pub fn start<S: Source + 'static>(&self, source: Arc<S>) -> Option<Running> {
        let directory = self.directory.clone()?;
        let task = tokio::spawn(run(
            source,
            directory,
            self.settings.subscribe(),
            Published {
                status: self.status.clone(),
                ready: self.ready.clone(),
            },
        ));
        Some(Running(Some(task)))
    }

    /// Hands the task `config`'s settings, after a reload. Turned off, runners get no archives from then on; turned
    /// on again, not before the task checked them.
    pub fn reload(&self, config: &Config) {
        if !config.caches.actions.enabled {
            self.ready.send_replace(None);
        }
        self.settings.send_if_modified(|settings| {
            let new = Settings::of(config);
            let changed = *settings != new;
            *settings = new;
            changed
        });
    }

    /// What clients show of the store: `None` while it is off or has not opened.
    pub fn status(&self) -> Option<api::ActionArchives> {
        let on = self.settings.borrow().enabled;
        self.status.borrow().clone().filter(|_| on)
    }

    /// What a runner container mounts and its environment gets, read once as it starts: the archives' directory,
    /// read-only, at [`MOUNT`], and [`ENV`] naming it, while the store is on and its last check found it good.
    pub fn runner(&self) -> Option<RunnerArchives> {
        if !self.settings.borrow().enabled {
            return None;
        }
        self.ready.borrow().clone()
    }
}

impl RunnerArchives {
    /// What a runner gets of the archives at `archives`.
    fn of(archives: &Path) -> RunnerArchives {
        RunnerArchives {
            mount: Mount {
                target: Some(MOUNT.to_owned()),
                source: Some(archives.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(true),
                ..Default::default()
            },
            env: format!("{ENV}={MOUNT}"),
        }
    }
}

/// What the task publishes to its `Link`.
struct Published {
    status: watch::Sender<Option<api::ActionArchives>>,
    ready: watch::Sender<Option<RunnerArchives>>,
}

/// What a runner container gets of the archives.
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerArchives {
    /// The archives' directory, read-only.
    pub mount: Mount,
    /// `ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE=/opt/runnerd/actions`.
    pub env: String,
}

/// The running task; dropping it stops the task. A download stopped midway leaves a file in `staging/`, which the
/// next start removes. The store's lock is held until the last blocking hash, check or write it started ends, as each
/// holds the store.
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

/// Owns the store at `directory`: refreshes at start, every `REFRESH` and whenever the settings change. Each
/// refresh opens the store if it is not open yet, checks it, and publishes the runners' view only if that check
/// found it good, before it downloads anything; then publishes what clients show.
async fn run<S: Source + 'static>(
    source: Arc<S>,
    directory: PathBuf,
    mut settings: watch::Receiver<Settings>,
    published: Published,
) {
    let mut archives = Archives::new(directory.clone());
    let view = RunnerArchives::of(&directory.join(store::ARCHIVES));
    loop {
        let now = settings.borrow_and_update().clone();
        if now.enabled {
            let good = archives.check(unix_now()).await;
            published.ready.send_replace(good.then(|| view.clone()));
            archives.update(&*source, &now, unix_now()).await;
            published.status.send_replace(Some(archives.status()));
        } else {
            published.ready.send_replace(None);
        }
        tokio::select! {
            () = tokio::time::sleep(REFRESH) => {}
            changed = settings.changed() => if changed.is_err() { return },
        }
    }
}

/// The store and what the task knows besides it: what a refresh works on.
#[derive(Debug)]
pub(crate) struct Archives {
    /// The store's directory.
    directory: PathBuf,
    /// Shared with the blocking tasks that hash, verify and write; only this task's refresh uses them, one at a time.
    /// `None` until it opens: each refresh tries again.
    store: Option<Arc<Mutex<ArchiveStore>>>,
    refresher: Refresher,
    limits: Limits,
}

impl Archives {
    /// The archives kept at `directory`, opened at the first check.
    pub(crate) fn new(directory: PathBuf) -> Archives {
        Archives {
            directory,
            store: None,
            refresher: Refresher::default(),
            limits: LIMITS,
        }
    }

    /// These archives, refreshed within `limits`.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, limits: Limits) -> Archives {
        self.limits = limits;
        self
    }

    /// What clients show.
    pub(crate) fn status(&self) -> api::ActionArchives {
        let (entries, bytes) = self
            .store
            .as_ref()
            .map_or((0, 0), |store| lock(store).size());
        self.refresher.status(entries, bytes)
    }

    /// One refresh at `now`: a check, then an update.
    #[cfg(test)]
    pub(crate) async fn refresh<S: Source>(&mut self, source: &S, settings: &Settings, now: u64) {
        self.check(now).await;
        self.update(source, settings, now).await;
    }

    /// Reads the pins of each served repository whose default branch moved, downloads and verifies what is pinned
    /// and missing, following the pins of each archive as deep as a runner nests actions, and removes what no
    /// repository pinned for `keep`, but only when every served repository's pins and every archive they reach are
    /// known whole: what was not read may pin anything. Nothing while the store is not open.
    pub(crate) async fn update<S: Source>(&mut self, source: &S, settings: &Settings, now: u64) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let meter = Meter::new(self.limits);
        let mut names = Names::default();
        let mut directs: BTreeMap<String, Option<Direct>> = BTreeMap::new();
        for repository in &settings.repositories {
            let direct = self
                .read_pins(source, &meter, &store, &mut names, repository, now)
                .await;
            directs.insert(repository.clone(), direct);
        }
        self.withdraw(source, &meter, &store, &mut names).await;
        // Each archive is tried once a refresh: one that failed waits for the next.
        let mut tried: BTreeSet<Key> = BTreeSet::new();
        // Each round downloads what the last made known, one level deeper: level 0, the repositories' own pins, to
        // level `MAX_DEPTH`.
        for _ in 0..=MAX_DEPTH {
            let mut wanted = BTreeSet::new();
            for direct in directs.values().flatten() {
                let reach = closure(source, &meter, &store, &mut names, &direct.wanted).await;
                wanted.extend(reach.keys);
            }
            let missing: Vec<Key> = wanted
                .iter()
                .filter(|key| !tried.contains(*key) && !lock(&store).holds(key))
                .cloned()
                .collect();
            if missing.is_empty() {
                break;
            }
            tried.extend(missing.iter().cloned());
            self.fetch_all(source, &meter, &store, missing, now).await;
        }
        let mut whole = BTreeMap::new();
        let mut removal = store::Removal::Expired;
        let mut wanted = BTreeSet::new();
        for (repository, direct) in &directs {
            let Some(direct) = direct else {
                removal = store::Removal::Held;
                continue;
            };
            let reach = closure(source, &meter, &store, &mut names, &direct.wanted).await;
            if direct.read && reach.complete {
                whole.insert(
                    repository.clone(),
                    store::Inventory {
                        head: direct.head.clone(),
                        wanted: direct.wanted.clone(),
                        reached: reach.keys.clone(),
                    },
                );
            } else {
                removal = store::Removal::Held;
            }
            wanted.extend(reach.keys);
        }
        if removal == store::Removal::Held {
            info!(
                "some pins of the served repositories are not known whole; no action archive is removed this refresh"
            );
        }
        let retained = {
            let store = store.clone();
            let keep = settings.keep;
            let served = settings.repositories.clone();
            let reached = wanted.clone();
            tokio::task::spawn_blocking(move || {
                let mut store = lock(&store);
                store.record(whole, &served)?;
                // What the last whole inventories reached stays too: while a repository cannot be read, what it
                // pinned when it last could is all that is known of it.
                let mut pinned = store.reached(&served);
                pinned.extend(reached);
                store.retain(&pinned, keep, removal, now)
            })
            .await
        };
        match retained {
            Ok(Ok(removed)) => {
                for key in removed {
                    info!(
                        "action archive {} removed: no repository pinned it for {}",
                        key.label(),
                        api::duration(settings.keep.as_secs())
                    );
                }
            }
            Ok(Err(error)) => warn!("cannot remove action archives no longer pinned: {error}"),
            Err(_) => {}
        }
        self.refresher.finish(&settings.repositories, &wanted, now);
    }

    /// Opens the store if it is not open, and checks every kept archive against the index: what changed is
    /// quarantined, and fetched again by the next update. Whether every file under `archives/` is now good: `false`
    /// while the store cannot open, or the check could not read or move a file.
    pub(crate) async fn check(&mut self, now: u64) -> bool {
        let subject = self.directory.display().to_string();
        let store = match self.open().await {
            Ok(store) => store,
            Err(error) => {
                warn!("no action archives: cannot open {subject}: {error}");
                self.refresher
                    .fail(&subject, api::ArchiveFailureKind::Store, now);
                return false;
            }
        };
        let checked = match tokio::task::spawn_blocking(move || lock(&store).check(now)).await {
            Ok(Ok(checked)) => checked,
            Ok(Err(error)) => {
                warn!("cannot check the action archives: {error}");
                self.refresher
                    .fail(&subject, api::ArchiveFailureKind::Store, now);
                return false;
            }
            Err(_) => return false,
        };
        for key in &checked.changed {
            self.refresher
                .fail(&key.label(), api::ArchiveFailureKind::Quarantined, now);
        }
        for stranger in &checked.strangers {
            warn!("action archive {stranger} is not in the index; moved to quarantine");
        }
        for error in &checked.errors {
            warn!("cannot check the action archive {error}; runners get no archives until it is");
        }
        if checked.clean() {
            self.refresher.succeed(&subject);
        } else {
            self.refresher
                .fail(&subject, api::ArchiveFailureKind::Store, now);
        }
        checked.clean()
    }

    /// Removes every kept archive whose repository GitHub now says is not public, or not there: anything a runner
    /// reads is what anyone may download, whether or not a repository still pins it. A repository GitHub does not
    /// answer for is left as it is.
    async fn withdraw<S: Source>(
        &mut self,
        source: &S,
        meter: &Meter,
        store: &Arc<Mutex<ArchiveStore>>,
        names: &mut Names,
    ) {
        let repositories = lock(store).repositories();
        let mut hidden = Vec::new();
        for repository in repositories {
            if let Ok(None) = names.public(source, meter, &repository).await {
                hidden.push(repository);
            }
        }
        if hidden.is_empty() {
            return;
        }
        let store = store.clone();
        let withdrawn = tokio::task::spawn_blocking(move || lock(&store).withdraw(&hidden)).await;
        match withdrawn {
            Ok(Ok(keys)) => {
                for key in keys {
                    info!(
                        "action archive {} removed: its repository is not public",
                        key.label()
                    );
                }
            }
            Ok(Err(error)) => warn!("cannot remove action archives no longer public: {error}"),
            Err(_) => {}
        }
    }

    /// The store, opened now if it is not yet.
    async fn open(&mut self) -> std::io::Result<Arc<Mutex<ArchiveStore>>> {
        if let Some(store) = &self.store {
            return Ok(store.clone());
        }
        let directory = self.directory.clone();
        let store = tokio::task::spawn_blocking(move || ArchiveStore::open(&directory))
            .await
            .map_err(std::io::Error::other)??;
        let store = Arc::new(Mutex::new(store));
        self.store = Some(store.clone());
        Ok(store)
    }

    /// The pins of `repository` this refresh goes by: read now when its default branch moved since its last whole
    /// inventory, that inventory's when it has not. When they cannot be read, the last inventory's, not read now, or
    /// `None` without one.
    async fn read_pins<S: Source>(
        &mut self,
        source: &S,
        meter: &Meter,
        store: &Arc<Mutex<ArchiveStore>>,
        names: &mut Names,
        repository: &str,
        now: u64,
    ) -> Option<Direct> {
        let last = lock(store).inventory(repository).cloned();
        match pins_of(source, meter, names, repository, last.as_ref()).await {
            Ok(direct) => {
                self.refresher.succeed(repository);
                Some(direct)
            }
            Err(error) => {
                warn!(
                    "{repository}: cannot read the actions it pins; keeping the last read and every archive: {}",
                    error.brief()
                );
                self.refresher
                    .fail(repository, api::ArchiveFailureKind::Pins, now);
                last.map(|inventory| Direct {
                    head: inventory.head,
                    wanted: inventory.wanted,
                    read: false,
                })
            }
        }
    }

    /// Downloads, verifies and publishes each of `missing`, a few at a time; returns the bytes downloaded.
    async fn fetch_all<S: Source>(
        &mut self,
        source: &S,
        meter: &Meter,
        store: &Arc<Mutex<ArchiveStore>>,
        missing: Vec<Key>,
        now: u64,
    ) {
        let results: Vec<(Key, Result<u64, Fetch>)> = futures_util::stream::iter(missing)
            .map(|key| async move {
                let result = fetch(source, meter, store, &key, now).await;
                (key, result)
            })
            .buffer_unordered(DOWNLOADS)
            .collect()
            .await;
        for (key, result) in results {
            let label = key.label();
            match result {
                Ok(_) => self.refresher.succeed(&label),
                Err(Fetch::Deferred) => {}
                Err(Fetch::Download(error)) => {
                    warn!(
                        "cannot download the action archive {label}; jobs download it themselves: {}",
                        error.brief()
                    );
                    self.refresher
                        .fail(&label, api::ArchiveFailureKind::Download, now);
                }
                Err(Fetch::Rejected(why)) => {
                    warn!(
                        "the action archive {label} {why}; not kept, jobs download it themselves"
                    );
                    self.refresher
                        .fail(&label, api::ArchiveFailureKind::Rejected, now);
                }
                Err(Fetch::Store(error)) => {
                    warn!("cannot keep the action archive {label}: {error}");
                    self.refresher
                        .fail(&label, api::ArchiveFailureKind::Download, now);
                }
            }
        }
    }
}

/// A served repository's own pins, as one refresh goes by them.
#[derive(Debug)]
struct Direct {
    /// The commit of its default branch they are of.
    head: String,
    wanted: BTreeSet<Wanted>,
    /// Whether they are known whole at `head`: read now, or the last whole inventory at the same head.
    read: bool,
}

/// `repository`'s pins at its default branch's head: the inventory `last`'s when the head has not moved, read whole
/// from its tree when it has. A tree GitHub cut short, more files than are read, a file too large or not YAML, and a
/// repository a pin names that GitHub does not answer for each fail it: what was not read may pin anything.
async fn pins_of<S: Source>(
    source: &S,
    meter: &Meter,
    names: &mut Names,
    repository: &str,
    last: Option<&store::Inventory>,
) -> Result<Direct, PinsError> {
    meter.ask(source)?;
    let details = source.action_repository(repository).await?;
    meter.ask(source)?;
    let head = source
        .branch_head(repository, &details.default_branch)
        .await?;
    if let Some(last) = last.filter(|last| last.head == head) {
        return Ok(Direct {
            head,
            wanted: last.wanted.clone(),
            read: true,
        });
    }
    meter.ask(source)?;
    let tree = source.tree(repository, &head).await?;
    if tree.truncated {
        return Err(PinsError::Truncated);
    }
    let files: Vec<&TreeFile> = tree
        .files
        .iter()
        .filter(|file| pins::read_for_pins(&file.path))
        .collect();
    if files.len() > PIN_FILES {
        return Err(PinsError::Files(files.len()));
    }
    let mut pinned: BTreeSet<Pin> = BTreeSet::new();
    for file in files {
        if file.size > PIN_FILE_BYTES {
            return Err(PinsError::Large(file.path.clone()));
        }
        meter.ask(source)?;
        let bytes = source.blob(repository, &file.blob, PIN_FILE_BYTES).await?;
        let found = pins::file_pins(&file.path, &bytes).map_err(|why| PinsError::Unreadable {
            path: file.path.clone(),
            why,
        })?;
        pinned.extend(found);
    }
    let mut wanted = BTreeSet::new();
    for pin in pinned {
        if let Some(name) = names.public(source, meter, &pin.repository).await? {
            wanted.insert(Wanted {
                key: Key {
                    repository: name,
                    sha: pin.sha,
                },
                path: pin.path,
            });
        }
    }
    Ok(Direct {
        head,
        wanted,
        read: true,
    })
}

/// Every archive `direct` reaches, through the pins of the archives held: level 0 is `direct`, and the pins of each
/// level's archives are the next, to level `MAX_DEPTH`, the deepest a runner nests.
#[derive(Debug)]
struct Reach {
    keys: BTreeSet<Key>,
    /// Whether every pin on the way is known: each archive is held, its action file was read, and each repository
    /// it pins answered.
    complete: bool,
}

/// What `direct` reaches now (`Reach`).
async fn closure<S: Source>(
    source: &S,
    meter: &Meter,
    store: &Arc<Mutex<ArchiveStore>>,
    names: &mut Names,
    direct: &BTreeSet<Wanted>,
) -> Reach {
    let mut seen: BTreeSet<Wanted> = BTreeSet::new();
    let mut complete = true;
    let mut level: BTreeSet<Wanted> = direct.clone();
    for depth in 0..=MAX_DEPTH {
        let mut next = BTreeSet::new();
        for wanted in &level {
            if !seen.insert(wanted.clone()) {
                continue;
            }
            let (held, unread, nested) = {
                let store = lock(store);
                (
                    store.holds(&wanted.key),
                    store.unread(&wanted.key, &wanted.path),
                    store.uses(&wanted.key, &wanted.path).to_vec(),
                )
            };
            if !held || unread {
                complete = false;
                continue;
            }
            // A runner nests no deeper: what the deepest level pins, it never runs.
            if depth == MAX_DEPTH {
                continue;
            }
            for pin in nested {
                match names.public(source, meter, &pin.repository).await {
                    Ok(Some(name)) => {
                        next.insert(Wanted {
                            key: Key {
                                repository: name,
                                sha: pin.sha,
                            },
                            path: pin.path,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        complete = false;
                        warn!(
                            "cannot read {}, which {} pins: {}",
                            pin.repository,
                            wanted.key.label(),
                            error.brief()
                        );
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    Reach {
        keys: seen.into_iter().map(|wanted| wanted.key).collect(),
        complete,
    }
}

/// Why an archive is not kept.
#[derive(Debug)]
enum Fetch {
    Download(Error),
    Rejected(String),
    Store(std::io::Error),
    /// Not tried, or not again: the refresh's bytes are spent, or GitHub's rate limit is.
    Deferred,
}

/// What one refresh may still spend: requests to GitHub's API, and bytes downloaded. Each download reserves the
/// largest archive's bytes before it starts and gives back what it did not use once it ends, whether it succeeded
/// or not, so the downloads in flight together never pass what is left.
#[derive(Debug)]
struct Meter {
    limits: Limits,
    requests: Mutex<u32>,
    bytes: Mutex<u64>,
}

impl Meter {
    fn new(limits: Limits) -> Meter {
        Meter {
            limits,
            requests: Mutex::new(limits.requests),
            bytes: Mutex::new(limits.bytes),
        }
    }

    /// Takes one request to GitHub's API, if the refresh and the rate limit have one to spare.
    fn ask<S: Source>(&self, source: &S) -> Result<(), Error> {
        if source.spare_requests().is_some_and(|spare| spare < RESERVE) {
            return Err(Error::Held(
                "what is left of GitHub's rate limit is kept for runners",
            ));
        }
        let mut left = lock(&self.requests);
        *left = left
            .checked_sub(1)
            .ok_or(Error::Held("this refresh's requests to GitHub are spent"))?;
        Ok(())
    }

    /// Reserves a download's bytes, if the refresh has them left.
    fn reserve(&self) -> bool {
        let mut left = lock(&self.bytes);
        let reserved = *left >= self.limits.archive;
        if reserved {
            *left -= self.limits.archive;
        }
        reserved
    }

    /// Gives back what a download reserved and did not use, `used` being what it wrote.
    fn spend(&self, used: u64) {
        *lock(&self.bytes) += self.limits.archive - used.min(self.limits.archive);
    }
}

/// Downloads `key`'s archive into `staging/`, a few attempts, verifies it whole, and publishes it. Whatever fails, its
/// staged file goes.
async fn fetch<S: Source>(
    source: &S,
    meter: &Meter,
    store: &Arc<Mutex<ArchiveStore>>,
    key: &Key,
    now: u64,
) -> Result<u64, Fetch> {
    let file = lock(store).staging_file().map_err(Fetch::Store)?;
    let mut last = None;
    for wait in ATTEMPTS {
        tokio::time::sleep(wait).await;
        if !meter.reserve() {
            return Err(Fetch::Deferred);
        }
        let result = source
            .tarball(&key.repository, &key.sha, &file, meter.limits.archive)
            .await;
        let used = match &result {
            Ok(size) => *size,
            Err(_) => tokio::fs::metadata(&file)
                .await
                .map_or(0, |metadata| metadata.len()),
        };
        meter.spend(used);
        match result {
            Ok(_) => {
                last = None;
                break;
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&file).await;
                if matches!(error, Error::RateLimited { .. } | Error::Held(_)) {
                    return Err(Fetch::Deferred);
                }
                let gone = error.is_gone();
                last = Some(error);
                // Asking again gets the same answer.
                if gone {
                    break;
                }
            }
        }
    }
    if let Some(error) = last {
        return Err(Fetch::Download(error));
    }
    let published = {
        let store = store.clone();
        let key = key.clone();
        let file = file.clone();
        tokio::task::spawn_blocking(move || -> Result<u64, Fetch> {
            let verified = verify::verify(&file, &key.sha, verify::BUDGET)
                .map_err(|rejected| Fetch::Rejected(format!("failed its check: {rejected}")))?;
            let size = verified.size;
            let mut uses: BTreeMap<String, Vec<Pin>> = BTreeMap::new();
            let mut unread = BTreeSet::new();
            for (dir, action) in &verified.actions {
                match action.bytes.as_deref().map(pins::action_file_pins) {
                    Some(Ok(found)) if found.is_empty() => {}
                    Some(Ok(found)) => {
                        uses.insert(dir.clone(), found);
                    }
                    Some(Err(_)) | None => {
                        unread.insert(dir.clone());
                    }
                }
            }
            let staged = Staged {
                file: file.clone(),
                sha256: verified.sha256,
                size,
                uses,
                unread,
            };
            lock(&store)
                .publish(key, staged, now)
                .map_err(Fetch::Store)?;
            Ok(size)
        })
        .await
    };
    let result = match published {
        Ok(result) => result,
        Err(error) => Err(Fetch::Store(std::io::Error::other(error))),
    };
    if result.is_err() {
        let _ = tokio::fs::remove_file(&file).await;
    }
    result
}

/// Why a repository's pins could not be read.
#[derive(Debug, thiserror::Error)]
enum PinsError {
    #[error(transparent)]
    Github(#[from] Error),
    #[error("GitHub cut its tree short")]
    Truncated,
    #[error("it has {0} workflows and action files, more than are read")]
    Files(usize),
    #[error("{0} is larger than is read")]
    Large(String),
    #[error("{path}: {why}")]
    Unreadable { path: String, why: pins::Unreadable },
}

impl PinsError {
    /// What failed, as a log line about another's repository says it (`Error::brief`).
    fn brief(&self) -> String {
        match self {
            PinsError::Github(error) => error.brief(),
            PinsError::Truncated
            | PinsError::Files(_)
            | PinsError::Large(_)
            | PinsError::Unreadable { .. } => self.to_string(),
        }
    }
}

/// The canonical names of the repositories pins name, as asked in one refresh: `None` for one that is not public or
/// not there.
#[derive(Debug, Default)]
struct Names(HashMap<String, Option<String>>);

impl Names {
    /// `repository`'s canonical name, if it is public and its name is one a file may be named by.
    async fn public<S: Source>(
        &mut self,
        source: &S,
        meter: &Meter,
        repository: &str,
    ) -> Result<Option<String>, Error> {
        let asked = repository.to_ascii_lowercase();
        if let Some(name) = self.0.get(&asked) {
            return Ok(name.clone());
        }
        meter.ask(source)?;
        let name = match source.action_repository(repository).await {
            Ok(found) => {
                (found.public && pins::repository(&found.full_name)).then_some(found.full_name)
            }
            Err(error) if error.is_gone() => None,
            Err(error) => return Err(error),
        };
        self.0.insert(asked, name.clone());
        Ok(name)
    }
}

#[cfg(test)]
mod tests;
