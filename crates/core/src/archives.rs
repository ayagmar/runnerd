//! The action archives runnerd keeps for its runners (ADR 0147): it reads which actions each served repository's
//! default branch pins by commit, downloads each public one's archive through GitHub's API, verifies it, and keeps it
//! where every runner reads it, read-only, as `ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE`, so a job copies it instead of
//! downloading it from codeload.github.com. An archive it does not hold, a job downloads itself, as without it.
//!
//! One task owns the store (`run`), off the scheduling path: it refreshes at start and every `REFRESH`, reading a
//! repository's tree and files only when its default branch moved. It never fetches a private action (runnerd's
//! token is not the job's, and every repository's runners read the same directory), never takes what to fetch from a
//! job's log, and never sets `ACTIONS_RUNNER_SYMLINK_CACHED_ACTIONS`. The token stays in `github::Github`.

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

/// How often the store refreshes: each repository's default branch is checked this often, one conditional request
/// each, which costs GitHub's rate limit nothing while it is unchanged.
const REFRESH: Duration = Duration::from_secs(600);

/// The largest archive downloaded: far above any action's.
const MAX_ARCHIVE: u64 = 256 << 20;

/// The most bytes one refresh downloads; what is left waits for the next.
const REFRESH_BYTES: u64 = 2 << 30;

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
    /// Downloads the archive of `repository` at `commit` (`GET /repos/{owner}/{repo}/tarball/{sha}`) into the file
    /// `to`, at most `limit` bytes, and returns its size.
    fn tarball(
        &self,
        repository: &str,
        commit: &str,
        to: &Path,
        limit: u64,
    ) -> impl Future<Output = Result<u64, Error>> + Send;
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
}

impl Archives {
    /// The archives kept at `directory`, opened at the first check.
    pub(crate) fn new(directory: PathBuf) -> Archives {
        Archives {
            directory,
            store: None,
            refresher: Refresher::default(),
        }
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
    /// repository pinned for `keep`. Nothing while the store is not open.
    pub(crate) async fn update<S: Source>(&mut self, source: &S, settings: &Settings, now: u64) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let mut names = Names::default();
        for repository in &settings.repositories {
            self.read_pins(source, &mut names, repository, now).await;
        }
        let mut fetched_bytes = 0;
        let mut wanted = BTreeSet::new();
        // Each archive is tried once a refresh: one that failed waits for the next.
        let mut tried: BTreeSet<Key> = BTreeSet::new();
        // Each round downloads what the last made known, one level deeper.
        for _ in 0..=MAX_DEPTH {
            wanted = self.closure(source, &store, &mut names).await;
            let missing: Vec<Key> = wanted
                .iter()
                .filter(|key| !tried.contains(*key) && !lock(&store).holds(key))
                .cloned()
                .collect();
            if missing.is_empty() || fetched_bytes >= REFRESH_BYTES {
                break;
            }
            tried.extend(missing.iter().cloned());
            fetched_bytes += self.fetch_all(source, &store, missing, now).await;
        }
        let retained = {
            let store = store.clone();
            let pinned = wanted.clone();
            let keep = settings.keep;
            tokio::task::spawn_blocking(move || lock(&store).retain(&pinned, keep, now)).await
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

    /// Reads which actions `repository`'s default branch pins, when it moved since the last read. On any failure,
    /// or a tree GitHub cut short, the last pins read are kept.
    async fn read_pins<S: Source>(
        &mut self,
        source: &S,
        names: &mut Names,
        repository: &str,
        now: u64,
    ) {
        match self.pins_of(source, names, repository).await {
            Ok(Some((head, wanted))) => {
                self.refresher.read(repository, head, wanted);
                self.refresher.succeed(repository);
            }
            Ok(None) => self.refresher.succeed(repository),
            Err(error) => {
                warn!(
                    "{repository}: cannot read the actions it pins; keeping the last read: {error}"
                );
                self.refresher
                    .fail(repository, api::ArchiveFailureKind::Pins, now);
            }
        }
    }

    /// `repository`'s head and the public actions its workflows and action files pin there, or `None` when its
    /// default branch has not moved since they were last read.
    async fn pins_of<S: Source>(
        &self,
        source: &S,
        names: &mut Names,
        repository: &str,
    ) -> Result<Option<(String, BTreeSet<Wanted>)>, PinsError> {
        let details = source.action_repository(repository).await?;
        let head = source
            .branch_head(repository, &details.default_branch)
            .await?;
        if self.refresher.head(repository) == Some(head.as_str()) {
            return Ok(None);
        }
        let tree = source.tree(repository, &head).await?;
        if tree.truncated {
            return Err(PinsError::Truncated);
        }
        let mut pinned: BTreeSet<Pin> = BTreeSet::new();
        let files = tree
            .files
            .iter()
            .filter(|file| pins::read_for_pins(&file.path) && file.size <= PIN_FILE_BYTES)
            .take(PIN_FILES);
        for file in files {
            let bytes = source.blob(repository, &file.blob, PIN_FILE_BYTES).await?;
            pinned.extend(pins::pins(&String::from_utf8_lossy(&bytes)));
        }
        let mut wanted = BTreeSet::new();
        for pin in pinned {
            if let Some(name) = names.public(source, &pin.repository).await? {
                wanted.insert(Wanted {
                    key: Key {
                        repository: name,
                        sha: pin.sha,
                    },
                    path: pin.path,
                });
            }
        }
        Ok(Some((head, wanted)))
    }

    /// Every archive the served repositories' pins reach, through the pins of the archives held, as deep as a runner
    /// nests actions. A nested action whose repository cannot be read now is left out of this refresh.
    async fn closure<S: Source>(
        &self,
        source: &S,
        store: &Arc<Mutex<ArchiveStore>>,
        names: &mut Names,
    ) -> BTreeSet<Key> {
        let mut seen: BTreeSet<Wanted> = BTreeSet::new();
        let mut level: BTreeSet<Wanted> = self.refresher.wanted();
        for _ in 0..MAX_DEPTH {
            let mut next = BTreeSet::new();
            for wanted in &level {
                if !seen.insert(wanted.clone()) {
                    continue;
                }
                let nested: Vec<Pin> = lock(store).uses(&wanted.key, &wanted.path).to_vec();
                for pin in nested {
                    match names.public(source, &pin.repository).await {
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
                        Err(error) => warn!(
                            "cannot read {}, which {} pins: {error}",
                            pin.repository,
                            wanted.key.label()
                        ),
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            level = next;
        }
        seen.into_iter().map(|wanted| wanted.key).collect()
    }

    /// Downloads, verifies and publishes each of `missing`, a few at a time; returns the bytes downloaded.
    async fn fetch_all<S: Source>(
        &mut self,
        source: &S,
        store: &Arc<Mutex<ArchiveStore>>,
        missing: Vec<Key>,
        now: u64,
    ) -> u64 {
        let results: Vec<(Key, Result<u64, Fetch>)> = futures_util::stream::iter(missing)
            .map(|key| async move {
                let result = fetch(source, store, &key, now).await;
                (key, result)
            })
            .buffer_unordered(DOWNLOADS)
            .collect()
            .await;
        let mut bytes = 0;
        for (key, result) in results {
            let label = key.label();
            match result {
                Ok(size) => {
                    bytes += size;
                    self.refresher.succeed(&label);
                }
                Err(Fetch::Download(error)) => {
                    warn!(
                        "cannot download the action archive {label}; jobs download it themselves: {error}"
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
        bytes
    }
}

/// Why an archive is not kept.
#[derive(Debug)]
enum Fetch {
    Download(Error),
    Rejected(String),
    Store(std::io::Error),
}

/// Downloads `key`'s archive into `staging/`, a few attempts, verifies it whole, and publishes it. Whatever fails, its
/// staged file goes.
async fn fetch<S: Source>(
    source: &S,
    store: &Arc<Mutex<ArchiveStore>>,
    key: &Key,
    now: u64,
) -> Result<u64, Fetch> {
    let file = lock(store).staging_file().map_err(Fetch::Store)?;
    let mut last = None;
    for wait in ATTEMPTS {
        tokio::time::sleep(wait).await;
        match source
            .tarball(&key.repository, &key.sha, &file, MAX_ARCHIVE)
            .await
        {
            Ok(_) => {
                last = None;
                break;
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&file).await;
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
            let uses: BTreeMap<String, Vec<Pin>> = verified
                .actions
                .iter()
                .map(|(dir, text)| (dir.clone(), pins::pins(text)))
                .filter(|(_, pins)| !pins.is_empty())
                .collect();
            let staged = Staged {
                file: file.clone(),
                sha256: verified.sha256,
                size,
                uses,
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
}

/// The canonical names of the repositories pins name, as asked in one refresh: `None` for one that is not public or
/// not there.
#[derive(Debug, Default)]
struct Names(HashMap<String, Option<String>>);

impl Names {
    async fn public<S: Source>(
        &mut self,
        source: &S,
        repository: &str,
    ) -> Result<Option<String>, Error> {
        let asked = repository.to_ascii_lowercase();
        if let Some(name) = self.0.get(&asked) {
            return Ok(name.clone());
        }
        let name = match source.action_repository(repository).await {
            Ok(found) => found.public.then_some(found.full_name),
            Err(error) if error.is_gone() => None,
            Err(error) => return Err(error),
        };
        self.0.insert(asked, name.clone());
        Ok(name)
    }
}

#[cfg(test)]
mod tests;
