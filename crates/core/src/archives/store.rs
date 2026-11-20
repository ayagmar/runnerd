//! The action archives on disk and their index (ADR 0147): `archives/<owner>_<repo>/<sha>.tar.gz`, laid out as the
//! runner looks them up and the only directory runners mount, `staging/` for downloads in progress, `quarantine/` for
//! archives that no longer match, and `index.json`. The index is what the store should hold and the source of
//! integrity: each entry's SHA-256 and size as verified, when it was fetched and when a refresh last found it
//! pinned. File times mean nothing to it.
//!
//! It must never write an archive in place, replace one that exists, or remove one but by retention or quarantine.
//! It survives a restart whole: the index is written to a temporary file, synced and renamed; an archive is synced,
//! renamed into place without replacing, and its directory synced before the index names it. A file the index does
//! not name, as one a crash left between its rename and the index's, is quarantined at the next check; `staging/`
//! is emptied at open. A reload changes nothing here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ring::rand::{SecureRandom, SystemRandom};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::pins::Pin;
use super::verify::hash_file;

/// The directory runners mount: archives only.
pub(crate) const ARCHIVES: &str = "archives";
const STAGING: &str = "staging";
const QUARANTINE: &str = "quarantine";
const INDEX: &str = "index.json";

/// An archive as the runner names it: the repository GitHub resolves the action to, and the commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Key {
    /// `owner/repo` as GitHub's API names it, case and all: the runner's `ResolvedNameWithOwner`.
    pub(crate) repository: String,
    /// The commit, 40 lowercase hex digits.
    pub(crate) sha: String,
}

impl Key {
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
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    entries: Vec<Entry>,
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
}

/// What a check of the kept archives found and moved to quarantine.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Checked {
    /// Archives the index names whose file no longer matched it: moved to quarantine, dropped from the index, to be
    /// fetched again.
    pub(crate) changed: Vec<Key>,
    /// Files under `archives/` the index does not name, by their path there: moved to quarantine.
    pub(crate) strangers: Vec<String>,
}

/// The store. Its owner is the archives task alone.
#[derive(Debug)]
pub(crate) struct ArchiveStore {
    root: PathBuf,
    entries: BTreeMap<Key, Entry>,
}

impl ArchiveStore {
    /// Opens the store at `root`, making its directories, emptying `staging/` and reading the index. An index that
    /// does not parse is read as empty, so every archive is quarantined at the first check and fetched again.
    pub(crate) fn open(root: &Path) -> io::Result<ArchiveStore> {
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
        Ok(ArchiveStore {
            root: root.to_owned(),
            entries: index
                .entries
                .into_iter()
                .map(|entry| (entry.key.clone(), entry))
                .collect(),
        })
    }

    /// The directory runners mount.
    pub(crate) fn archives(&self) -> PathBuf {
        self.root.join(ARCHIVES)
    }

    /// Whether it holds `key`.
    pub(crate) fn holds(&self, key: &Key) -> bool {
        self.entries.contains_key(key)
    }

    /// What the archive `key` pins in turn, for its action at `path`.
    pub(crate) fn uses(&self, key: &Key, path: &str) -> &[Pin] {
        self.entries
            .get(key)
            .and_then(|entry| entry.uses.get(path))
            .map_or(&[], Vec::as_slice)
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

    /// Checks every kept archive against the index, and moves to quarantine each whose file is not what was verified
    /// and each file the index does not name. An archive whose file is gone is dropped from the index, to be fetched
    /// again.
    pub(crate) fn check(&mut self, now: u64) -> io::Result<Checked> {
        let mut checked = Checked::default();
        let archives = self.archives();
        let mut gone = Vec::new();
        for (key, entry) in &self.entries {
            let path = archives.join(key.file());
            match hash_file(&path) {
                Ok((sha256, size)) if sha256 == entry.sha256 && size == entry.size => {}
                Ok(_) => {
                    quarantine(&self.root, &path, now)?;
                    checked.changed.push(key.clone());
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => gone.push(key.clone()),
                Err(error) => return Err(error),
            }
        }
        let known: BTreeSet<PathBuf> = self.entries.keys().map(Key::file).collect();
        for dir in fs::read_dir(&archives)? {
            let dir = dir?;
            let inside = if dir.file_type()?.is_dir() {
                fs::read_dir(dir.path())?
                    .map(|file| file.map(|file| file.path()))
                    .collect::<io::Result<Vec<_>>>()?
            } else {
                vec![dir.path()]
            };
            for file in inside {
                let relative = file.strip_prefix(&archives).unwrap_or(&file).to_owned();
                if !known.contains(&relative) {
                    quarantine(&self.root, &file, now)?;
                    checked
                        .strangers
                        .push(relative.to_string_lossy().into_owned());
                }
            }
        }
        if !checked.changed.is_empty() || !gone.is_empty() {
            for key in checked.changed.iter().chain(&gone) {
                self.entries.remove(key);
            }
            self.save()?;
        }
        Ok(checked)
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
            },
        );
        self.save()
    }

    /// Notes that a refresh at `now` found every archive of `pinned` pinned, and removes each archive no refresh has
    /// found pinned for `keep`. Returns those removed. A runner copying one as it goes downloads its action itself.
    pub(crate) fn retain(
        &mut self,
        pinned: &BTreeSet<Key>,
        keep: Duration,
        now: u64,
    ) -> io::Result<Vec<Key>> {
        let mut removed = Vec::new();
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
        // Empty repository directories go too, as nothing in them is a runner's any more.
        for dir in fs::read_dir(&archives)? {
            let dir = dir?.path();
            if dir.is_dir() && fs::read_dir(&dir)?.next().is_none() {
                fs::remove_dir(&dir)?;
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
