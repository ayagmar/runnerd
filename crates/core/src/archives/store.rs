//! The action archives on disk and their index (ADR 0147): `archives/<owner>_<repo>/<sha>.tar.gz`, laid out as the
//! runner looks them up and the only directory runners mount, `staging/` for downloads in progress, `quarantine/` for
//! archives that no longer match, and `index.json`. The index is what the store should hold and the source of
//! integrity: each entry's SHA-256 and size as verified, when it was fetched and when a refresh last found it
//! pinned, and what its action files pin. It also keeps each served repository's pins as last read, so an unchanged
//! head is never read again, and what they reached when all were last known, so a restart while a repository cannot be
//! read keeps what it pinned; the archives that failed their check or are not there to anyone, so they are not
//! downloaded again; and the repositories jobs resolved as immutable packages, never kept. File times mean nothing to
//! it.
//!
//! One store has one owner: `open` takes an exclusive lock on `<root>/lock` and holds it for as long as the store
//! lives, so a second runnerd pointed at the same directory is refused rather than sharing it. An index entry whose
//! repository or commit is not a name GitHub gives is dropped at open: no path is ever made of it.
//!
//! It must never write an archive in place, replace one that exists, or remove one but by retention, quarantine or a
//! withdrawal.
//! It survives a restart whole: the index is written to a temporary file, synced and renamed; an archive is synced,
//! renamed into place without replacing, and its directory synced before the index names it. A file the index does
//! not name, as one a crash left between its rename and the index's, is quarantined at the next check; `staging/`
//! is emptied at open. A reload changes nothing here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ring::rand::{SecureRandom, SystemRandom};
use rustix::fs::{CWD, FlockOperation, RenameFlags, flock, renameat_with};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::pins::Pin;
use super::verify::hash_file;

/// The directory runners mount: archives only.
pub(crate) const ARCHIVES: &str = "archives";
const STAGING: &str = "staging";
const QUARANTINE: &str = "quarantine";
const INDEX: &str = "index.json";
const LOCK: &str = "lock";

/// An archive as the runner names it: the repository GitHub resolves the action to, and the commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Key {
    /// `owner/repo` as GitHub's API names it, case and all: the runner's `ResolvedNameWithOwner`.
    pub(crate) repository: String,
    /// The commit, 40 lowercase hex digits.
    pub(crate) sha: String,
}

impl Key {
    /// Whether it names an archive as GitHub would: `owner/repo` of safe names, and a 40-digit lowercase commit.
    pub(crate) fn valid(&self) -> bool {
        super::pins::repository(&self.repository) && super::pins::commit(&self.sha)
    }

    /// Its file under `archives/`: the repository's `/` as `_`, as the runner replaces it.
    fn file(&self) -> PathBuf {
        Path::new(&self.repository.replace('/', "_")).join(format!("{}.tar.gz", self.sha))
    }

    /// `owner/repo@sha`, as failures name it.
    pub(crate) fn label(&self) -> String {
        format!("{}@{}", self.repository, self.sha)
    }
}

/// What the index keeps of an archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    key: Key,
    /// Of the file as verified, lowercase hex.
    sha256: String,
    size: u64,
    /// Seconds since the epoch.
    fetched_at: u64,
    /// When a refresh last found it pinned, in seconds since the epoch.
    pinned_at: u64,
    /// The actions it pins in turn, by the directory of the action file that pins them (`""` for its root).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    uses: BTreeMap<String, Vec<Pin>>,
    /// The directories whose action file could not be read: what they pin is not known.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    unread: BTreeSet<String>,
    /// The directories that hold an action file at all.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    actions: BTreeSet<String>,
}

/// A served repository's own pins as last read whole: what its workflows and action files pin at a commit of its
/// default branch, before GitHub is asked which are public. Read again only when the branch moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Scan {
    /// The commit it was read at.
    pub(crate) head: String,
    pub(crate) pins: BTreeSet<Pin>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    entries: Vec<Entry>,
    /// By served repository, its last scan.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    scans: BTreeMap<String, Scan>,
    /// By served repository, every archive its pins reached the last time all of them were known.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    reached: BTreeMap<String, BTreeSet<Key>>,
    /// Archives that failed their check: the same commit's archive fails it again, so it is not downloaded again.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    refused: BTreeSet<Key>,
    /// Repositories a job resolved as an immutable action package, lower case: never kept.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    packages: BTreeSet<String>,
}

/// What an archive holds for an action at a path, as a refresh follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// The store does not hold the archive.
    Missing,
    /// It holds no `action.yml` or `action.yaml` there: no action a runner loads.
    NoFile,
    /// Its action file there is not one runnerd reads, so what it pins is not followed.
    Unread,
    /// What its action file there pins.
    Pins(Vec<Pin>),
}

/// An archive verified in `staging/`, ready to publish.
#[derive(Debug)]
pub(crate) struct Staged {
    /// Its file in `staging/`.
    pub(crate) file: PathBuf,
    /// Of the file, lowercase hex.
    pub(crate) sha256: String,
    pub(crate) size: u64,
    /// What it pins in turn, by the directory of the action file that pins them.
    pub(crate) uses: BTreeMap<String, Vec<Pin>>,
    /// The directories whose action file could not be read.
    pub(crate) unread: BTreeSet<String>,
    /// The directories that hold an action file.
    pub(crate) actions: BTreeSet<String>,
}

/// What a check of the kept archives found and moved to quarantine.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Checked {
    /// Archives the index names whose file no longer matched it: moved to quarantine, dropped from the index, to be
    /// fetched again.
    pub(crate) changed: Vec<Key>,
    /// Files under `archives/` the index does not name, by their path there: moved to quarantine.
    pub(crate) strangers: Vec<String>,
    /// What could not be read or moved, each with why. While any is left, what runners would read is not known good.
    pub(crate) errors: Vec<String>,
}

impl Checked {
    /// Whether every file under `archives/` is now one the index names and matches.
    pub(crate) fn clean(&self) -> bool {
        self.errors.is_empty()
    }
}

/// How a kept archive's file stands against its entry.
enum Standing {
    Matches,
    Changed,
    Gone,
}

/// What noting a package did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Packaged {
    /// Whether it was not noted before.
    pub(crate) new: bool,
    /// The archives of it removed.
    pub(crate) removed: Vec<Key>,
}

/// The store. Its owner is the archives task alone, which the lock it holds makes the only one on this host.
#[derive(Debug)]
pub(crate) struct ArchiveStore {
    root: PathBuf,
    entries: BTreeMap<Key, Entry>,
    scans: BTreeMap<String, Scan>,
    reached: BTreeMap<String, BTreeSet<Key>>,
    refused: BTreeSet<Key>,
    packages: BTreeSet<String>,
    /// `<root>/lock`, locked exclusively until the store is dropped.
    _lock: File,
}

impl ArchiveStore {
    /// Opens the store at `root`, making it and its parents, taking its lock, emptying `staging/` and reading the
    /// index. Fails with `WouldBlock` while another owner holds the lock. An index that does not parse is read as
    /// empty, so every archive is quarantined at the first check and fetched again.
    pub(crate) fn open(root: &Path) -> io::Result<ArchiveStore> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(root.join(LOCK))?;
        lock_exclusive(&lock)?;
        for dir in [root.to_owned(), root.join(ARCHIVES), root.join(QUARANTINE)] {
            make_dir(&dir)?;
        }
        let staging = root.join(STAGING);
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        make_dir(&staging)?;
        let index = match fs::read(root.join(INDEX)) {
            Ok(bytes) => serde_json::from_slice::<Index>(&bytes).unwrap_or_else(|error| {
                warn!("the action archives' index does not parse; every archive is checked again: {error}");
                Index::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Index::default(),
            Err(error) => return Err(error),
        };
        let (valid, invalid): (Vec<Entry>, Vec<Entry>) = index
            .entries
            .into_iter()
            .partition(|entry| entry.key.valid());
        if !invalid.is_empty() {
            warn!(
                "the action archives' index names {} archives that are not GitHub's names; they are dropped",
                invalid.len()
            );
        }
        Ok(ArchiveStore {
            root: root.to_owned(),
            entries: valid
                .into_iter()
                .map(|entry| (entry.key.clone(), entry))
                .collect(),
            scans: index
                .scans
                .into_iter()
                .filter(|(repository, _)| super::pins::repository(repository))
                .collect(),
            reached: index
                .reached
                .into_iter()
                .filter(|(repository, keys)| {
                    super::pins::repository(repository) && keys.iter().all(Key::valid)
                })
                .collect(),
            refused: index.refused.into_iter().filter(Key::valid).collect(),
            packages: index.packages,
            _lock: lock,
        })
    }

    /// The directory runners mount.
    pub(crate) fn archives(&self) -> PathBuf {
        self.root.join(ARCHIVES)
    }

    /// What the archive `key` holds for its action at `path`.
    pub(crate) fn action(&self, key: &Key, path: &str) -> Action {
        let Some(entry) = self.entries.get(key) else {
            return Action::Missing;
        };
        if entry.unread.contains(path) {
            Action::Unread
        } else if !entry.actions.contains(path) && !entry.uses.contains_key(path) {
            Action::NoFile
        } else {
            Action::Pins(entry.uses.get(path).cloned().unwrap_or_default())
        }
    }

    /// Whether `key`'s archive failed its check before.
    pub(crate) fn refused(&self, key: &Key) -> bool {
        self.refused.contains(key)
    }

    /// Notes that `key`'s archive failed its check.
    pub(crate) fn refuse(&mut self, key: Key) -> io::Result<()> {
        if self.refused.insert(key) {
            self.save()?;
        }
        Ok(())
    }

    /// Whether a job resolved `repository` as an immutable action package.
    pub(crate) fn package(&self, repository: &str) -> bool {
        self.packages.contains(&repository.to_ascii_lowercase())
    }

    /// Notes that a job resolved `repository` as an immutable action package, and removes every archive of it, file
    /// and entry: the runner would read one in the package's place. Returns those removed, and whether it is new.
    pub(crate) fn packaged(&mut self, repository: &str) -> io::Result<Packaged> {
        let lower = repository.to_ascii_lowercase();
        let new = self.packages.insert(lower.clone());
        let held: Vec<String> = self
            .repositories()
            .into_iter()
            .filter(|held| held.to_ascii_lowercase() == lower)
            .collect();
        let removed = self.withdraw(&held)?;
        if new {
            self.save()?;
        }
        Ok(Packaged { new, removed })
    }

    /// The repositories jobs resolved as packages, lower case.
    pub(crate) fn packages(&self) -> Vec<String> {
        self.packages.iter().cloned().collect()
    }

    /// `repository`'s last scan.
    pub(crate) fn scan(&self, repository: &str) -> Option<&Scan> {
        self.scans.get(repository)
    }

    /// Keeps `scan` as `repository`'s last.
    pub(crate) fn scanned(&mut self, repository: &str, scan: Scan) -> io::Result<()> {
        if self.scans.get(repository) != Some(&scan) {
            self.scans.insert(repository.to_owned(), scan);
            self.save()?;
        }
        Ok(())
    }

    /// Every archive `repository`'s pins reached the last time all of them were known.
    pub(crate) fn reached(&self, repository: &str) -> BTreeSet<Key> {
        self.reached.get(repository).cloned().unwrap_or_default()
    }

    /// Keeps `whole`, by served repository every archive its pins reach now that all of them are known, and forgets
    /// the scans and reaches of repositories no longer `served`.
    pub(crate) fn record(
        &mut self,
        whole: BTreeMap<String, BTreeSet<Key>>,
        served: &[String],
    ) -> io::Result<()> {
        self.reached.extend(whole);
        self.reached
            .retain(|repository, _| served.contains(repository));
        self.scans
            .retain(|repository, _| served.contains(repository));
        self.save()
    }

    /// The repositories it holds archives of, each once.
    pub(crate) fn repositories(&self) -> Vec<String> {
        let mut repositories: Vec<String> = self
            .entries
            .keys()
            .map(|key| key.repository.clone())
            .collect();
        repositories.dedup();
        repositories
    }

    /// Removes every archive of `repositories`, file and entry. Returns those removed.
    pub(crate) fn withdraw(&mut self, repositories: &[String]) -> io::Result<Vec<Key>> {
        let removed: Vec<Key> = self
            .entries
            .keys()
            .filter(|key| repositories.contains(&key.repository))
            .cloned()
            .collect();
        let archives = self.archives();
        for key in &removed {
            match fs::remove_file(archives.join(key.file())) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            self.entries.remove(key);
        }
        self.save()?;
        Ok(removed)
    }

    /// How many archives it holds, and their bytes.
    pub(crate) fn size(&self) -> (u64, u64) {
        (
            self.entries.len() as u64,
            self.entries.values().map(|entry| entry.size).sum(),
        )
    }

    /// A new file name in `staging/` for a download.
    pub(crate) fn staging_file(&self) -> io::Result<PathBuf> {
        let mut bytes = [0_u8; 12];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| io::Error::other("no random bytes"))?;
        let name = super::verify::hex(&bytes);
        Ok(self.root.join(STAGING).join(name))
    }

    /// Checks every kept archive against the index, and moves to quarantine each whose file is not what was verified,
    /// whatever sits there (a directory, a link), and each file the index does not name. An archive whose file is gone
    /// is dropped from the index, to be fetched again. One file it cannot read or move does not stop it: it goes on
    /// with the rest, saves what it changed, and says what it could not do in `errors`.
    pub(crate) fn check(&mut self, now: u64) -> io::Result<Checked> {
        let mut checked = Checked::default();
        let archives = self.archives();
        let keys: Vec<Key> = self.entries.keys().cloned().collect();
        let mut dropped = false;
        for key in keys {
            let path = archives.join(key.file());
            let standing = match self.standing(&key, &path) {
                Ok(standing) => standing,
                Err(error) => {
                    checked.errors.push(format!("{}: {error}", key.label()));
                    continue;
                }
            };
            match standing {
                Standing::Matches => {}
                Standing::Gone => {
                    self.entries.remove(&key);
                    dropped = true;
                }
                Standing::Changed => match quarantine(&self.root, &path, now) {
                    Ok(()) => {
                        self.entries.remove(&key);
                        dropped = true;
                        checked.changed.push(key);
                    }
                    Err(error) => checked.errors.push(format!("{}: {error}", key.label())),
                },
            }
        }
        let known: BTreeSet<PathBuf> = self.entries.keys().map(Key::file).collect();
        for (relative, file) in strangers(&archives, &known, &mut checked.errors)? {
            match quarantine(&self.root, &file, now) {
                Ok(()) => checked.strangers.push(relative),
                Err(error) => checked.errors.push(format!("{relative}: {error}")),
            }
        }
        if dropped {
            self.save()?;
        }
        Ok(checked)
    }

    /// How `key`'s file at `path` stands: anything but a regular file has changed.
    fn standing(&self, key: &Key, path: &Path) -> io::Result<Standing> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Standing::Gone),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() {
            return Ok(Standing::Changed);
        }
        let entry = &self.entries[key];
        let (sha256, size) = hash_file(path)?;
        Ok(if sha256 == entry.sha256 && size == entry.size {
            Standing::Matches
        } else {
            Standing::Changed
        })
    }

    /// Makes `staged` the archive `key`: synced, renamed into place without replacing a file there, its directory
    /// synced, then named in the index. An archive already there stays as it is: the staged file is removed.
    pub(crate) fn publish(&mut self, key: Key, staged: Staged, now: u64) -> io::Result<()> {
        if self.entries.contains_key(&key) {
            return fs::remove_file(&staged.file);
        }
        fs::set_permissions(&staged.file, fs::Permissions::from_mode(0o644))?;
        File::open(&staged.file)?.sync_all()?;
        let target = self.archives().join(key.file());
        let dir = target
            .parent()
            .map_or_else(|| self.archives(), Path::to_owned);
        if !dir.exists() {
            make_dir(&dir)?;
            sync_dir(&self.archives())?;
        }
        renameat_with(CWD, &staged.file, CWD, &target, RenameFlags::NOREPLACE)?;
        sync_dir(&dir)?;
        self.entries.insert(
            key.clone(),
            Entry {
                key,
                sha256: staged.sha256,
                size: staged.size,
                fetched_at: now,
                pinned_at: now,
                uses: staged.uses,
                unread: staged.unread,
                actions: staged.actions,
            },
        );
        self.save()
    }

    /// Notes that a refresh at `now` found every archive of `pinned` pinned, and removes each archive no refresh has
    /// found pinned for `keep`. Returns those removed. A runner copying one as it goes downloads its action itself.
    /// Quarantined files go after `keep` too, and refusals of archives no longer pinned.
    pub(crate) fn retain(
        &mut self,
        pinned: &BTreeSet<Key>,
        keep: Duration,
        now: u64,
    ) -> io::Result<Vec<Key>> {
        let mut removed = Vec::new();
        self.refused.retain(|key| pinned.contains(key));
        for (key, entry) in &mut self.entries {
            if pinned.contains(key) {
                entry.pinned_at = now;
            } else if now.saturating_sub(entry.pinned_at) > keep.as_secs() {
                removed.push(key.clone());
            }
        }
        let archives = self.archives();
        for key in &removed {
            match fs::remove_file(archives.join(key.file())) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            self.entries.remove(key);
        }
        // Empty repository directories go too, as nothing in them is a runner's any more. A link is never followed.
        for dir in fs::read_dir(&archives)? {
            let dir = dir?;
            if dir.file_type()?.is_dir() && fs::read_dir(dir.path())?.next().is_none() {
                fs::remove_dir(dir.path())?;
            }
        }
        // Quarantined files are kept for `keep` too, to be looked at.
        prune_quarantine(&self.root, keep, now)?;
        self.save()?;
        Ok(removed)
    }

    /// Writes the index: to a temporary file, synced, renamed over the last, the directory synced.
    fn save(&self) -> io::Result<()> {
        let index = Index {
            entries: self.entries.values().cloned().collect(),
            scans: self.scans.clone(),
            reached: self.reached.clone(),
            refused: self.refused.clone(),
            packages: self.packages.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&index).map_err(io::Error::other)?;
        let temporary = self.root.join(format!("{INDEX}.tmp"));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, self.root.join(INDEX))?;
        sync_dir(&self.root)
    }
}

/// How long `open` waits for the lock: a process forked from this one holds it until it runs its program, which it
/// does at once; another owner holds it for good.
const LOCK_TRIES: u32 = 40;
const LOCK_WAIT: Duration = Duration::from_millis(25);

/// Takes `lock` exclusively, waiting out a moment's hold. Fails with `WouldBlock` while another owner holds it.
fn lock_exclusive(lock: &File) -> io::Result<()> {
    for _ in 1..LOCK_TRIES {
        match flock(lock, FlockOperation::NonBlockingLockExclusive) {
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => std::thread::sleep(LOCK_WAIT),
            other => return other.map_err(io::Error::from),
        }
    }
    flock(lock, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error == rustix::io::Errno::WOULDBLOCK {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "another runnerd keeps its action archives here",
            )
        } else {
            error.into()
        }
    })
}

/// Every file under `archives/` that is not one of `known`'s, as its path there and in full: anything at its top that
/// is not a directory, and anything in a repository's directory that is not a known regular file. Links are never
/// followed. What cannot be read is said in `errors` and left.
fn strangers(
    archives: &Path,
    known: &BTreeSet<PathBuf>,
    errors: &mut Vec<String>,
) -> io::Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();
    for top in fs::read_dir(archives)? {
        let top = top?;
        let inside = match top.file_type() {
            Ok(kind) if kind.is_dir() => match fs::read_dir(top.path()) {
                Ok(inside) => inside,
                Err(error) => {
                    errors.push(format!("{}: {error}", top.path().display()));
                    continue;
                }
            },
            Ok(_) => {
                found.push((top.file_name().to_string_lossy().into_owned(), top.path()));
                continue;
            }
            Err(error) => {
                errors.push(format!("{}: {error}", top.path().display()));
                continue;
            }
        };
        for file in inside {
            let file = match file {
                Ok(file) => file,
                Err(error) => {
                    errors.push(format!("{}: {error}", top.path().display()));
                    continue;
                }
            };
            let path = file.path();
            let relative = path.strip_prefix(archives).unwrap_or(&path).to_owned();
            let regular = file.file_type().is_ok_and(|kind| kind.is_file());
            if !regular || !known.contains(&relative) {
                found.push((relative.to_string_lossy().into_owned(), path));
            }
        }
    }
    Ok(found)
}

/// Makes `dir` with mode 0755 whatever the umask, if it is not there.
fn make_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o755).create(dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Moves `file` to `quarantine/`, named by when and what it was: `<now>-<owner>_<repo>-<sha>.tar.gz`.
fn quarantine(root: &Path, file: &Path, now: u64) -> io::Result<()> {
    let parent = file
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // The same file may be quarantined twice in one second: the second gets a number, never replaces the first.
    for attempt in 0_u32.. {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!(".{attempt}")
        };
        let target = root
            .join(QUARANTINE)
            .join(format!("{now}-{parent}-{name}{suffix}"));
        match renameat_with(CWD, file, CWD, &target, RenameFlags::NOREPLACE) {
            Ok(()) => {
                warn!(
                    "action archive {parent}/{name} does not match what was verified; moved to {}",
                    target.display()
                );
                return Ok(());
            }
            Err(error) if error == rustix::io::Errno::EXIST && attempt < 1000 => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Removes what was quarantined more than `keep` before `now`, by the time its name starts with.
fn prune_quarantine(root: &Path, keep: Duration, now: u64) -> io::Result<()> {
    for file in fs::read_dir(root.join(QUARANTINE))? {
        let file = file?;
        let name = file.file_name().to_string_lossy().into_owned();
        let at: Option<u64> = name.split('-').next().and_then(|at| at.parse().ok());
        if at.is_none_or(|at| now.saturating_sub(at) > keep.as_secs()) {
            let path = file.path();
            if file.file_type()?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}
