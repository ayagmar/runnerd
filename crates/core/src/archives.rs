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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bollard::models::{Mount, MountType};
use futures_util::StreamExt;
use tokio::sync::watch;
use tracing::{info, warn};

use self::pins::{MAX_DEPTH, Pin};
use self::state::{Refresher, Wanted};
use self::store::{Action, ArchiveStore, Key, Staged};
use crate::api;
use crate::config::Config;
use crate::github::{self, Error};
use crate::sync::lock;

/// Where a runner container finds the archives.
pub const MOUNT: &str = "/opt/runnerd/actions";

/// The label of a runner container that mounts the archives: what a restart counts it by.
pub const LABEL: &str = "runnerd.action-archives";

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
    /// The most requests one refresh sends to GitHub's API; what is left waits for the next.
    pub(crate) requests: u32,
    /// The most bytes the store holds: a download that could pass it waits until retention makes room.
    pub(crate) store: u64,
}

/// The limits runnerd refreshes by.
const LIMITS: Limits = Limits {
    archive: 256 << 20,
    bytes: 2 << 30,
    requests: 1000,
    store: 8 << 30,
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

/// Which of GitHub's budgets a read spends (ADR 0181).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// A served repository's reads: with the token, on the limit runners' registrations and the scheduler share.
    Served,
    /// A pinned action's repository's read: on the token's limit too, or on an anonymous budget of its own.
    Public,
}

/// What the archives read from GitHub. `github::Github` is the real one; the token never leaves it.
pub trait Source: Send + Sync {
    /// A served repository's canonical name, visibility and default branch, read with the token: it may be private.
    fn served_repository(
        &self,
        repository: &str,
    ) -> impl Future<Output = Result<ActionRepository, Error>> + Send;
    /// A pinned action's repository's canonical name, visibility and default branch: another's, read with the token or
    /// anonymously. A 404 or 410 says it is not there to whoever asked; a limit used up, a refusal, an answer that does
    /// not parse and no answer say nothing about it.
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
    /// that is not public is not there. Adds every byte of the body it receives to `received`, the one past `limit`
    /// and a failed download's included.
    fn tarball(
        &self,
        repository: &str,
        commit: &str,
        to: &Path,
        limit: u64,
        received: &AtomicU64,
    ) -> impl Future<Output = Result<u64, Error>> + Send;
    /// The requests left of `budget` before it resets, as last read; `None` when no answer said, or when the budget
    /// is one the source holds itself to.
    fn spare_requests(&self, budget: Budget) -> Option<u64>;
    /// How a pinned action's repository is read (ADR 0181): anonymously, runners get the archives only once every kept
    /// archive's repository answered.
    fn public_reads(&self) -> github::PublicReads {
        github::PublicReads::Authenticated
    }
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

/// The engine's side of the store: where it is, the settings it hands the task, what the task last published (what
/// clients show, and what runners get), the runners a restart adopted with the archives mounted, and the
/// repositories jobs resolved as immutable action packages. The task publishes the runners' view only after the store
/// opened and a check found every file under `archives/` good, and withdraws it before every check and while the
/// archives are off; a runner reads it once as it starts. Nothing survives a restart: a new task checks before it
/// publishes, and the store keeps the packages.
#[derive(Debug)]
pub struct Link {
    /// The store's directory; `None` when the daemon could not name one.
    directory: Option<PathBuf>,
    settings: watch::Sender<Settings>,
    status: watch::Sender<Option<api::ActionArchives>>,
    /// What a runner gets while the archives are checked good; `None` otherwise.
    ready: watch::Sender<Option<RunnerArchives>>,
    /// Runners a restart adopted whose container mounts the archives, by name, until each is gone: they read a view
    /// no check of this daemon validated when they started. `None` until recovery is done, which the first check
    /// waits for.
    readers: watch::Sender<Option<BTreeSet<String>>>,
    /// Repositories jobs resolved as immutable action packages, as their logs named them.
    packages: Packages,
}

/// Where a job's log tells the archives task of an immutable action package (`record::immutable_packages`).
#[derive(Debug, Clone)]
pub struct Packages(watch::Sender<BTreeSet<String>>);

impl Packages {
    /// Notes the packages `lines`, stored lines of a job's log, show a runner downloading.
    pub fn read(&self, lines: &[String]) {
        let found = crate::record::immutable_packages(lines);
        if found.is_empty() {
            return;
        }
        self.0.send_if_modified(|known| {
            let before = known.len();
            known.extend(found.iter().map(|name| name.to_ascii_lowercase()));
            known.len() != before
        });
    }
}

impl Link {
    /// The link to a store at `directory`, going by `config`.
    pub fn new(directory: Option<PathBuf>, config: &Config) -> Link {
        Link {
            directory,
            settings: watch::Sender::new(Settings::of(config)),
            status: watch::Sender::new(None),
            ready: watch::Sender::new(None),
            readers: watch::Sender::new(None),
            packages: Packages(watch::Sender::new(BTreeSet::new())),
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
                readers: self.readers.subscribe(),
                packages: self.packages.0.subscribe(),
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

    /// Notes that a restart adopted `runner`, if its container's `labels` say it mounts the archives.
    pub fn adopted<S: std::hash::BuildHasher>(
        &self,
        runner: &str,
        labels: &HashMap<String, String, S>,
    ) {
        if !labels.contains_key(LABEL) {
            return;
        }
        self.readers.send_if_modified(|readers| {
            readers
                .get_or_insert_with(BTreeSet::new)
                .insert(runner.to_owned())
        });
    }

    /// Notes that recovery adopted every runner it will: the first check, which waits for this, then knows every
    /// runner that reads the archives unchecked.
    pub fn recovered(&self) {
        self.readers.send_if_modified(|readers| {
            let first = readers.is_none();
            readers.get_or_insert_with(BTreeSet::new);
            first
        });
    }

    /// Notes that `runner` is gone.
    pub fn released(&self, runner: &str) {
        self.readers.send_if_modified(|readers| {
            readers
                .as_mut()
                .is_some_and(|readers| readers.remove(runner))
        });
    }

    /// Where jobs' logs tell of immutable action packages.
    pub fn packages(&self) -> Packages {
        self.packages.clone()
    }

    /// What clients show of the store: `None` while it is off, or has not opened and no adopted runner reads it.
    pub fn status(&self) -> Option<api::ActionArchives> {
        if !self.settings.borrow().enabled {
            return None;
        }
        let readers = self.readers.borrow().clone().unwrap_or_default();
        match self.status.borrow().clone() {
            Some(mut status) => {
                // A runner that was exposed and is gone is no longer anyone's concern.
                status.failures.retain(|failure| {
                    failure.kind != api::ArchiveFailureKind::Exposed
                        || readers.contains(&failure.subject)
                });
                Some(status)
            }
            // Before the first check, every adopted reader read a view no check validated.
            None if !readers.is_empty() => Some(api::ActionArchives {
                unchecked: readers.len() as u64,
                ..api::ActionArchives::default()
            }),
            None => None,
        }
    }

    /// The runners' view as it changes. A receiver sees only the latest value: a withdrawal the task replaces before
    /// the receiver looks is missed, so a test that must see one holds the task inside its check.
    #[cfg(test)]
    pub(crate) fn ready_changes(&self) -> watch::Receiver<Option<RunnerArchives>> {
        self.ready.subscribe()
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

/// What the task publishes to its `Link`, and what it hears from it.
struct Published {
    status: watch::Sender<Option<api::ActionArchives>>,
    ready: watch::Sender<Option<RunnerArchives>>,
    readers: watch::Receiver<Option<BTreeSet<String>>>,
    packages: watch::Receiver<BTreeSet<String>>,
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

/// Keeps `bytes` in the store at `directory` as the archive of `repository` at `sha`, as a refresh left it: the next
/// check finds it matching, and the next refresh asks GitHub whether its repository is still public.
#[cfg(test)]
pub(crate) fn keep(
    directory: &Path,
    repository: &str,
    sha: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut store = ArchiveStore::open(directory)?;
    let file = store.staging_file()?;
    std::fs::write(&file, bytes)?;
    let (sha256, size) = verify::hash_file(&file)?;
    let key = Key {
        repository: repository.to_owned(),
        sha: sha.to_owned(),
    };
    let staged = Staged {
        file,
        sha256,
        size,
        uses: BTreeMap::new(),
        unread: BTreeSet::new(),
        actions: BTreeSet::new(),
    };
    store.publish(key, staged, unix_now())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Owns the store at `directory`: refreshes at start, every `REFRESH` and whenever the settings change, and withdraws
/// what jobs resolved as packages as soon as a log names one. Each refresh withdraws the runners' view, opens the
/// store if it is not open yet, checks it, withdraws what is no longer public or is a package, and publishes the view
/// again only if the check found it good and, when pinned actions are read anonymously, every kept archive's repository
/// answered (ADR 0181); then it downloads, and publishes what clients show.
async fn run<S: Source + 'static>(
    source: Arc<S>,
    directory: PathBuf,
    mut settings: watch::Receiver<Settings>,
    mut published: Published,
) {
    let mut archives = Archives::new(directory.clone());
    let view = RunnerArchives::of(&directory.join(store::ARCHIVES));
    // The first check names the runners recovery adopted, so it waits for recovery.
    if published.readers.wait_for(Option::is_some).await.is_err() {
        return;
    }
    let mut first = true;
    loop {
        let now = settings.borrow_and_update().clone();
        if now.enabled {
            // A start while the check runs gets nothing, rather than what the check may yet quarantine.
            published.ready.send_replace(None);
            let packages = published.packages.borrow_and_update().clone();
            github::REQUESTS
                .scope(std::cell::Cell::new(0), async {
                    let mut round = archives.round();
                    let checked = archives.check(unix_now()).await;
                    if first {
                        let readers = published.readers.borrow().clone().unwrap_or_default();
                        archives.exposed(&readers, checked, unix_now());
                        first = false;
                    }
                    archives.packages(&packages, unix_now()).await;
                    let vetted = archives.vet(&*source, &mut round, unix_now()).await;
                    let mountable = checked.mountable(vetted, source.public_reads());
                    if checked.good() && !mountable {
                        warn!(
                            "not every kept action archive's repository answered; runners get no archives until one \
                             refresh hears from all of them"
                        );
                    }
                    published
                        .ready
                        .send_replace(mountable.then(|| view.clone()));
                    published.status.send_replace(Some(archives.status()));
                    archives
                        .update(&*source, &mut round, &now, unix_now())
                        .await;
                })
                .await;
            published.status.send_replace(Some(archives.status()));
        } else {
            published.ready.send_replace(None);
        }
        loop {
            tokio::select! {
                () = tokio::time::sleep(REFRESH) => break,
                changed = settings.changed() => if changed.is_err() { return } else { break },
                changed = published.packages.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let packages = published.packages.borrow_and_update().clone();
                    if settings.borrow().enabled {
                        archives.packages(&packages, unix_now()).await;
                        published.status.send_replace(Some(archives.status()));
                    }
                }
            }
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

/// What one refresh spends and has asked: its meter and the names GitHub gave.
#[derive(Debug)]
pub(crate) struct Round {
    meter: Meter,
    names: Names,
}

/// What a check found, as the task acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checked {
    /// Every file is good, and none was moved.
    Good,
    /// Every file is good now, after the check moved some to quarantine.
    Moved,
    /// The store did not open, or a file could not be read or moved: what runners would read is not known good.
    Bad,
}

impl Checked {
    /// Whether runners may read the archives.
    pub(crate) fn good(self) -> bool {
        self != Checked::Bad
    }

    /// Whether runners may read the archives after this check and a vetting that found `vetted`, their pinned
    /// actions read as `public` says. Read with the token, a lookup left unanswered keeps the archives mounted, as
    /// ADR 0147 had it; read anonymously, on a budget that often runs out, it withdraws them until every kept
    /// archive's repository answered (ADR 0181). Either way the archives stay in the store.
    pub(crate) fn mountable(self, vetted: Vetted, public: github::PublicReads) -> bool {
        self.good() && (vetted == Vetted::Whole || public == github::PublicReads::Authenticated)
    }
}

/// What a vetting of the kept archives found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vetted {
    /// Every kept archive's repository answered, and what is not public was removed.
    Whole,
    /// A repository's lookup was left unanswered, or what is not public could not be removed.
    Partly,
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

    /// A new refresh's round.
    pub(crate) fn round(&self) -> Round {
        Round {
            meter: Meter::new(self.limits),
            names: Names::default(),
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

    /// One refresh at `now`, as the task runs it; returns whether the task would publish the runners' view.
    #[cfg(test)]
    pub(crate) async fn refresh<S: Source>(
        &mut self,
        source: &S,
        settings: &Settings,
        now: u64,
    ) -> bool {
        github::REQUESTS
            .scope(std::cell::Cell::new(0), async {
                let mut round = self.round();
                let checked = self.check(now).await;
                let vetted = self.vet(source, &mut round, now).await;
                self.update(source, &mut round, settings, now).await;
                checked.mountable(vetted, source.public_reads())
            })
            .await
    }

    /// One update at `now`, without the check before it.
    #[cfg(test)]
    pub(crate) async fn refill<S: Source>(&mut self, source: &S, settings: &Settings, now: u64) {
        github::REQUESTS
            .scope(std::cell::Cell::new(0), async {
                let mut round = self.round();
                self.update(source, &mut round, settings, now).await;
            })
            .await;
    }

    /// The store's directory, as failures of the store name it.
    fn subject(&self) -> String {
        self.directory.display().to_string()
    }

    /// Notes what the restart's first check means for `readers`, the runners it adopted with the archives mounted:
    /// when it moved or could not settle a file, each may have read it, and is named until it is gone.
    pub(crate) fn exposed(&mut self, readers: &BTreeSet<String>, checked: Checked, now: u64) {
        if readers.is_empty() {
            return;
        }
        if checked == Checked::Good {
            info!(
                "{} runners a restart adopted read the action archives; the first check found every one good",
                readers.len()
            );
            return;
        }
        for reader in readers {
            warn!(
                "{reader} kept running across the restart with the action archives mounted, and the first check found \
                 one changed or could not read one: its job may have read it"
            );
            self.refresher
                .fail(reader, api::ArchiveFailureKind::Exposed, now);
        }
    }

    /// Withdraws every archive of `packages`, repositories jobs resolved as immutable action packages, and keeps them
    /// out from then on.
    pub(crate) async fn packages(&mut self, packages: &BTreeSet<String>, now: u64) {
        let Some(store) = self.store.clone() else {
            return;
        };
        for package in packages {
            let noted = {
                let store = store.clone();
                let package = package.clone();
                tokio::task::spawn_blocking(move || lock(&store).packaged(&package)).await
            };
            match noted {
                Ok(Ok(withdrawn)) => {
                    for key in &withdrawn.removed {
                        info!(
                            "action archive {} removed: a job resolved its repository as an immutable package",
                            key.label()
                        );
                    }
                    if withdrawn.new {
                        self.refresher
                            .fail(package, api::ArchiveFailureKind::Package, now);
                    }
                }
                Ok(Err(error)) => {
                    warn!("cannot remove the action archives of the package {package}: {error}");
                    self.refresher
                        .fail(&self.subject(), api::ArchiveFailureKind::Store, now);
                }
                Err(_) => {}
            }
        }
    }

    /// Reads each served repository's pins, from its tree only when its default branch moved, asks GitHub which are
    /// public every time, downloads and verifies what is pinned and missing, following the pins of each archive as
    /// deep as a runner nests actions, and removes what no repository pinned for `keep`. A repository whose pins are
    /// not all known keeps every archive it last reached; an action that is not supported (not public, a package, an
    /// archive that failed its check, no action at its path) is known, and kept out. Nothing while the store is not
    /// open.
    pub(crate) async fn update<S: Source>(
        &mut self,
        source: &S,
        round: &mut Round,
        settings: &Settings,
        now: u64,
    ) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let mut directs: BTreeMap<String, Option<Direct>> = BTreeMap::new();
        for repository in &settings.repositories {
            let direct = self.read_pins(source, round, &store, repository, now).await;
            directs.insert(repository.clone(), direct);
        }
        // Each archive is tried once a refresh: one that failed waits for the next.
        let mut tried: BTreeSet<Key> = BTreeSet::new();
        // Those the refresh's budget or the store's cap left for later: missing, but not for want of reading.
        let mut deferred: BTreeSet<Key> = BTreeSet::new();
        // Each round downloads what the last made known, one level deeper: level 0, the repositories' own pins, to
        // level `MAX_DEPTH`.
        for _ in 0..=MAX_DEPTH {
            let mut wanted = BTreeSet::new();
            for direct in directs.values().flatten() {
                let reach = closure(source, round, &store, &direct.wanted).await;
                wanted.extend(reach.missing);
            }
            let missing: Vec<Key> = wanted.difference(&tried).cloned().collect();
            if missing.is_empty() {
                break;
            }
            tried.extend(missing.iter().cloned());
            deferred.extend(self.fetch_all(source, round, &store, missing, now).await);
        }
        let mut whole = BTreeMap::new();
        let mut pinned = BTreeSet::new();
        let mut unsupported = BTreeMap::new();
        let mut refused = BTreeSet::new();
        for (repository, direct) in &directs {
            let (complete, waiting) = match direct {
                Some(direct) => {
                    let reach = closure(source, round, &store, &direct.wanted).await;
                    unsupported.extend(reach.unsupported);
                    refused.extend(reach.refused);
                    pinned.extend(reach.keys.iter().cloned());
                    let read = direct.known && reach.answered;
                    let complete = read && reach.missing.is_empty();
                    let waiting = read && reach.missing.is_subset(&deferred);
                    if complete {
                        whole.insert(repository.clone(), reach.keys);
                    }
                    (complete, waiting)
                }
                None => (false, false),
            };
            if complete || waiting {
                self.refresher
                    .succeed_kind(repository, api::ArchiveFailureKind::Held);
            }
            if !complete {
                // What it reached when all its pins were last known stays: what was not read may pin it.
                pinned.extend(lock(&store).reached(repository));
                // Only waiting for downloads is no news: the next refresh goes on with them.
                if !waiting {
                    info!(
                        "{repository}: not all its pins are known; the archives it last reached are kept"
                    );
                    self.refresher
                        .fail(repository, api::ArchiveFailureKind::Held, now);
                }
            }
        }
        for (subject, kind) in &unsupported {
            self.refresher.fail(subject, *kind, now);
        }
        let retained = {
            let store = store.clone();
            let keep = settings.keep;
            let served = settings.repositories.clone();
            let pinned = pinned.clone();
            tokio::task::spawn_blocking(move || {
                let mut store = lock(&store);
                store.record(whole, &served)?;
                store.retain(&pinned, keep, now)
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
            Ok(Err(error)) => {
                warn!("cannot remove action archives no longer pinned: {error}");
                self.refresher
                    .fail(&self.subject(), api::ArchiveFailureKind::Store, now);
            }
            Err(_) => {}
        }
        let mut subjects: BTreeSet<String> = pinned.iter().map(Key::label).collect();
        subjects.extend(unsupported.into_keys());
        subjects.extend(refused);
        self.refresher
            .finish(&settings.repositories, &subjects, &self.subject(), now);
    }

    /// Opens the store if it is not open, and checks every kept archive against the index: what changed is
    /// quarantined, and fetched again by the next update.
    pub(crate) async fn check(&mut self, now: u64) -> Checked {
        let subject = self.subject();
        let store = match self.open().await {
            Ok(store) => store,
            Err(error) => {
                warn!("no action archives: cannot open {subject}: {error}");
                self.refresher
                    .fail(&subject, api::ArchiveFailureKind::Store, now);
                return Checked::Bad;
            }
        };
        // What jobs resolved as packages stays reported, a restart included.
        for package in lock(&store).packages() {
            self.refresher
                .fail(&package, api::ArchiveFailureKind::Package, now);
        }
        let checked = match tokio::task::spawn_blocking(move || lock(&store).check(now)).await {
            Ok(Ok(checked)) => checked,
            Ok(Err(error)) => {
                warn!("cannot check the action archives: {error}");
                self.refresher
                    .fail(&subject, api::ArchiveFailureKind::Store, now);
                return Checked::Bad;
            }
            Err(_) => return Checked::Bad,
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
        if !checked.clean() {
            self.refresher
                .fail(&subject, api::ArchiveFailureKind::Store, now);
            return Checked::Bad;
        }
        self.refresher.succeed(&subject);
        if checked.changed.is_empty() && checked.strangers.is_empty() {
            Checked::Good
        } else {
            Checked::Moved
        }
    }

    /// Removes every kept archive whose repository GitHub now says is not public, or not there, before runners get
    /// the view: anything a runner reads is what anyone may download, whether or not a repository still pins it. A
    /// repository GitHub does not answer for is left as it is, and the vetting is only `Partly` done.
    pub(crate) async fn vet<S: Source>(
        &mut self,
        source: &S,
        round: &mut Round,
        now: u64,
    ) -> Vetted {
        let Some(store) = self.store.clone() else {
            return Vetted::Whole;
        };
        let repositories = lock(&store).repositories();
        let mut hidden = Vec::new();
        let mut vetted = Vetted::Whole;
        for repository in repositories {
            match round
                .names
                .public(source, &round.meter, &store, &repository)
                .await
            {
                Ok(None) => hidden.push(repository),
                Ok(Some(_)) => {}
                Err(_) => vetted = Vetted::Partly,
            }
        }
        if hidden.is_empty() {
            return vetted;
        }
        let withdrawn = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || lock(&store).withdraw(&hidden)).await
        };
        match withdrawn {
            Ok(Ok(keys)) => {
                for key in keys {
                    info!(
                        "action archive {} removed: its repository is not public",
                        key.label()
                    );
                }
            }
            Ok(Err(error)) => {
                warn!("cannot remove action archives no longer public: {error}");
                self.refresher
                    .fail(&self.subject(), api::ArchiveFailureKind::Store, now);
                return Vetted::Partly;
            }
            Err(_) => return Vetted::Partly,
        }
        vetted
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

    /// The pins of `repository` this refresh goes by, each asked of GitHub for its name and visibility: its last
    /// scan's when its default branch has not moved, read from its tree when it has, and the last scan's, not known
    /// whole, when it cannot be read. `None` when it cannot be read and never was.
    async fn read_pins<S: Source>(
        &mut self,
        source: &S,
        round: &mut Round,
        store: &Arc<Mutex<ArchiveStore>>,
        repository: &str,
        now: u64,
    ) -> Option<Direct> {
        let last = lock(store).scan(repository).cloned();
        let (scan, known) = match scan_of(source, &round.meter, repository, last.as_ref()).await {
            Ok(scan) => {
                self.refresher.succeed(repository);
                if last.as_ref() != Some(&scan) {
                    let saved = {
                        let store = store.clone();
                        let repository = repository.to_owned();
                        let scan = scan.clone();
                        tokio::task::spawn_blocking(move || lock(&store).scanned(&repository, scan))
                            .await
                    };
                    if let Ok(Err(error)) = saved {
                        warn!("cannot keep {repository}'s pins: {error}");
                        self.refresher
                            .fail(&self.subject(), api::ArchiveFailureKind::Store, now);
                    }
                }
                (scan, true)
            }
            Err(error) => {
                warn!(
                    "{repository}: cannot read the actions it pins; keeping the last read and every archive they \
                     reached: {}",
                    error.brief()
                );
                self.refresher
                    .fail(repository, api::ArchiveFailureKind::Pins, now);
                (last?, false)
            }
        };
        let mut direct = Direct {
            wanted: BTreeSet::new(),
            known,
        };
        for pin in scan.pins {
            match round
                .names
                .public(source, &round.meter, store, &pin.repository)
                .await
            {
                Ok(Some(name)) => {
                    direct.wanted.insert(Wanted {
                        key: Key {
                            repository: name,
                            sha: pin.sha,
                        },
                        path: pin.path,
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        "cannot read {}, which {repository} pins: {}",
                        pin.repository,
                        error.brief()
                    );
                    direct.known = false;
                }
            }
        }
        Some(direct)
    }

    /// Downloads, verifies and publishes each of `missing`, a few at a time. Returns those left for a later refresh,
    /// by its budget or the store's cap.
    async fn fetch_all<S: Source>(
        &mut self,
        source: &S,
        round: &Round,
        store: &Arc<Mutex<ArchiveStore>>,
        missing: Vec<Key>,
        now: u64,
    ) -> BTreeSet<Key> {
        let mut deferred = BTreeSet::new();
        let meter = &round.meter;
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
                Err(Fetch::Deferred) => {
                    deferred.insert(key);
                }
                Err(Fetch::Full) => {
                    deferred.insert(key);
                    warn!(
                        "the action archives are at their size cap; {label} waits for room, and jobs download it \
                         themselves"
                    );
                    self.refresher
                        .fail(&self.subject(), api::ArchiveFailureKind::Full, now);
                }
                Err(Fetch::Download(error)) => {
                    warn!(
                        "cannot download the action archive {label}; jobs download it themselves: {}",
                        error.brief()
                    );
                    self.refresher
                        .fail(&label, api::ArchiveFailureKind::Download, now);
                }
                Err(Fetch::Gone) => {
                    warn!(
                        "the action archive {label} is not there to anyone; it is not asked for again while pinned"
                    );
                    self.refusal(store, key, now).await;
                    self.refresher
                        .fail(&label, api::ArchiveFailureKind::Download, now);
                }
                Err(Fetch::Rejected(why)) => {
                    warn!(
                        "the action archive {label} {why}; not kept, jobs download it themselves"
                    );
                    self.refusal(store, key, now).await;
                    self.refresher
                        .fail(&label, api::ArchiveFailureKind::Rejected, now);
                }
                Err(Fetch::Store(error)) => {
                    warn!("cannot keep the action archive {label}: {error}");
                    self.refresher
                        .fail(&self.subject(), api::ArchiveFailureKind::Store, now);
                }
            }
        }
        deferred
    }

    /// Notes that `key`'s archive is not one to download again.
    async fn refusal(&mut self, store: &Arc<Mutex<ArchiveStore>>, key: Key, now: u64) {
        let store = store.clone();
        if let Ok(Err(error)) = tokio::task::spawn_blocking(move || lock(&store).refuse(key)).await
        {
            warn!("cannot keep a refused action archive's note: {error}");
            self.refresher
                .fail(&self.subject(), api::ArchiveFailureKind::Store, now);
        }
    }
}

/// A served repository's own pins, as one refresh goes by them.
#[derive(Debug)]
struct Direct {
    /// Those public, by GitHub's names.
    wanted: BTreeSet<Wanted>,
    /// Whether they are known whole: its files read in full at its head, and every repository they name answered.
    known: bool,
}

/// `repository`'s pins at its default branch's head: `last`'s when the head has not moved, read whole from its tree
/// when it has. A tree GitHub cut short, more files than are read, and a file too large or not YAML each fail it: what
/// was not read may pin anything.
async fn scan_of<S: Source>(
    source: &S,
    meter: &Meter,
    repository: &str,
    last: Option<&store::Scan>,
) -> Result<store::Scan, PinsError> {
    meter.ask(source, Budget::Served)?;
    let details = source.served_repository(repository).await?;
    meter.ask(source, Budget::Served)?;
    let head = source
        .branch_head(repository, &details.default_branch)
        .await?;
    if let Some(last) = last.filter(|last| last.head == head) {
        return Ok(last.clone());
    }
    meter.ask(source, Budget::Served)?;
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
    let mut pins: BTreeSet<Pin> = BTreeSet::new();
    for file in files {
        if file.size > PIN_FILE_BYTES {
            return Err(PinsError::Large(file.path.clone()));
        }
        meter.ask(source, Budget::Served)?;
        let bytes = source.blob(repository, &file.blob, PIN_FILE_BYTES).await?;
        let found = pins::file_pins(&file.path, &bytes).map_err(|why| PinsError::Unreadable {
            path: file.path.clone(),
            why,
        })?;
        pins.extend(found);
    }
    Ok(store::Scan { head, pins })
}

/// What a repository's pins reach now, through the pins of the archives held: level 0 is its own, and the pins of
/// each level's archives are the next, to level `MAX_DEPTH`, the deepest a runner nests.
#[derive(Debug, Default)]
struct Reach {
    /// Every archive reached: held, to download, or refused.
    keys: BTreeSet<Key>,
    /// Those not held, to download: while any is left, what it pins is not known.
    missing: BTreeSet<Key>,
    /// Actions reached that are not supported, by subject: known, and kept out.
    unsupported: BTreeMap<String, api::ArchiveFailureKind>,
    /// Archives reached that failed their check or are not there to anyone, by label: known, and not downloaded
    /// again.
    refused: BTreeSet<String>,
    /// Whether every repository a nested pin names answered.
    answered: bool,
}

/// What `direct` reaches now (`Reach`).
async fn closure<S: Source>(
    source: &S,
    round: &mut Round,
    store: &Arc<Mutex<ArchiveStore>>,
    direct: &BTreeSet<Wanted>,
) -> Reach {
    let mut reach = Reach {
        answered: true,
        ..Reach::default()
    };
    let mut seen: BTreeSet<Wanted> = BTreeSet::new();
    let mut level: BTreeSet<Wanted> = direct.clone();
    for depth in 0..=MAX_DEPTH {
        let mut next = BTreeSet::new();
        for wanted in &level {
            if !seen.insert(wanted.clone()) {
                continue;
            }
            let (refused, action) = {
                let store = lock(store);
                (
                    store.refused(&wanted.key),
                    store.action(&wanted.key, &wanted.path),
                )
            };
            let subject = wanted.subject();
            let nested = match action {
                // Refused before: the runner downloads it itself, and runnerd does not again.
                Action::Missing if refused => {
                    // Still reached: its refusal stays while it is.
                    reach.keys.insert(wanted.key.clone());
                    reach.refused.insert(wanted.key.label());
                    continue;
                }
                Action::Missing => {
                    reach.keys.insert(wanted.key.clone());
                    reach.missing.insert(wanted.key.clone());
                    continue;
                }
                Action::NoFile => {
                    reach.keys.insert(wanted.key.clone());
                    reach
                        .unsupported
                        .insert(subject, api::ArchiveFailureKind::NotAction);
                    continue;
                }
                Action::Unread => {
                    reach.keys.insert(wanted.key.clone());
                    reach
                        .unsupported
                        .insert(subject, api::ArchiveFailureKind::Unreadable);
                    continue;
                }
                Action::Pins(nested) => {
                    reach.keys.insert(wanted.key.clone());
                    nested
                }
            };
            // A runner nests no deeper: what the deepest level pins, it never runs.
            if depth == MAX_DEPTH {
                continue;
            }
            for pin in nested {
                match round
                    .names
                    .public(source, &round.meter, store, &pin.repository)
                    .await
                {
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
                        reach.answered = false;
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
    reach
}

/// Why an archive is not kept.
#[derive(Debug)]
enum Fetch {
    Download(Error),
    /// Not there to anyone: codeload does not send it anonymously.
    Gone,
    Rejected(String),
    Store(std::io::Error),
    /// Not tried, or not again: the refresh's bytes are spent, or GitHub's rate limit is.
    Deferred,
    /// Not tried: the store is at its size cap.
    Full,
}

/// What one refresh may still spend: requests to GitHub's API, as sent (`github::REQUESTS`, a request sent again
/// with a new token included), and bytes received. Each download reserves the largest archive's bytes before it starts
/// and gives back what it did not receive once it ends, whether it succeeded or not, so the downloads in flight
/// together never pass what is left, nor, with what the store holds, its cap.
#[derive(Debug)]
struct Meter {
    limits: Limits,
    bytes: Arc<Mutex<Bytes>>,
}

/// The bytes a refresh has left to receive, and those downloads in flight may still add to the store.
#[derive(Debug)]
struct Bytes {
    left: u64,
    in_flight: u64,
}

/// Why a download may not start.
enum Refused {
    /// The refresh's bytes are spent.
    Spent,
    /// The store, with what is in flight, would pass its cap.
    Full,
}

/// A download's place in the store's cap, given back when it ends. Once its archive is published, it is given back
/// with the store still locked, so a download reserving with the store locked counts the archive once: in the store's
/// size or in flight.
struct InFlight {
    bytes: Arc<Mutex<Bytes>>,
    archive: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut bytes = lock(&self.bytes);
        bytes.in_flight = bytes.in_flight.saturating_sub(self.archive);
    }
}

impl Meter {
    fn new(limits: Limits) -> Meter {
        Meter {
            limits,
            bytes: Arc::new(Mutex::new(Bytes {
                left: limits.bytes,
                in_flight: 0,
            })),
        }
    }

    /// Lets one more request go to GitHub's API, if the refresh and `budget` have one to spare.
    fn ask<S: Source>(&self, source: &S, budget: Budget) -> Result<(), Error> {
        if source
            .spare_requests(budget)
            .is_some_and(|spare| spare < RESERVE)
        {
            return Err(Error::Held(
                "what is left of GitHub's rate limit is kept for runners",
            ));
        }
        // A request may be sent twice, once more with a new token after a 401.
        if github::requests_sent() + 2 > u64::from(self.limits.requests) {
            return Err(Error::Held("this refresh's requests to GitHub are spent"));
        }
        Ok(())
    }

    /// Reserves a download's bytes, if the refresh has them left and `store` has room for them beside the downloads in
    /// flight. The place in the cap is held until what it returns is dropped.
    fn reserve(&self, store: &ArchiveStore) -> Result<InFlight, Refused> {
        let held = store.size().1;
        let mut bytes = lock(&self.bytes);
        if held + bytes.in_flight + self.limits.archive > self.limits.store {
            return Err(Refused::Full);
        }
        if bytes.left < self.limits.archive {
            return Err(Refused::Spent);
        }
        bytes.left -= self.limits.archive;
        bytes.in_flight += self.limits.archive;
        Ok(InFlight {
            bytes: self.bytes.clone(),
            archive: self.limits.archive,
        })
    }

    /// Gives back what a download reserved and did not receive.
    fn spend(&self, received: u64) {
        lock(&self.bytes).left += self.limits.archive - received.min(self.limits.archive);
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
    // Held until the archive is published or given up on.
    let mut place = None;
    for wait in ATTEMPTS {
        tokio::time::sleep(wait).await;
        drop(place.take());
        // With the store locked: an archive another download publishes is in the store's size or in flight, not both.
        let reserved = meter.reserve(&lock(store));
        place = Some(match reserved {
            Ok(place) => place,
            Err(Refused::Full) => return Err(Fetch::Full),
            Err(Refused::Spent) => return Err(Fetch::Deferred),
        });
        let received = AtomicU64::new(0);
        let result = source
            .tarball(
                &key.repository,
                &key.sha,
                &file,
                meter.limits.archive,
                &received,
            )
            .await;
        meter.spend(received.load(Ordering::Relaxed));
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
                // Asking again gets the same answer.
                if error.is_gone() {
                    return Err(Fetch::Gone);
                }
                last = Some(error);
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
            let actions: BTreeSet<String> = verified.actions.keys().cloned().collect();
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
                actions,
            };
            let mut store = lock(&store);
            store.publish(key, staged, now).map_err(Fetch::Store)?;
            // Its bytes are in the store's size now: given back before another download can read that size.
            drop(place);
            drop(store);
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

/// The canonical names of the repositories pins name, as asked in one refresh: `None` for one that is not public, not
/// there, or a package.
#[derive(Debug, Default)]
struct Names(HashMap<String, Option<String>>);

impl Names {
    /// `repository`'s canonical name, if it is public, no job resolved it as a package, and its name is one a file
    /// may be named by.
    async fn public<S: Source>(
        &mut self,
        source: &S,
        meter: &Meter,
        store: &Arc<Mutex<ArchiveStore>>,
        repository: &str,
    ) -> Result<Option<String>, Error> {
        let asked = repository.to_ascii_lowercase();
        if let Some(name) = self.0.get(&asked) {
            return Ok(name.clone());
        }
        if lock(store).package(&asked) {
            self.0.insert(asked, None);
            return Ok(None);
        }
        meter.ask(source, Budget::Public)?;
        let name = match source.action_repository(repository).await {
            Ok(found) => (found.public
                && pins::repository(&found.full_name)
                && !lock(store).package(&found.full_name))
            .then_some(found.full_name),
            Err(error) if error.is_gone() => None,
            Err(error) => return Err(error),
        };
        self.0.insert(asked, name.clone());
        Ok(name)
    }
}

#[cfg(test)]
mod tests;
