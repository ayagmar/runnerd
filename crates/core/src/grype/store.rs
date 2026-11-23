//! The Grype database's directory (ADR 0160): one owner at a time by an exclusive lock, `staging/` for what is
//! downloaded and imported, `dbs/<generation>/` for each database Grype imported and checked, never written again once
//! there, and `state.json` naming the one runners mount. Only this type's methods write any of it.
//!
//! What survives a restart: the generations under `dbs/` and `state.json`; `staging/` is emptied at open, and the held
//! database is hashed against `state.json` before anything mounts it. A reload changes nothing here.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

/// Grype's database directory under its cache directory: its schema model, as `installation.Config` names it.
pub(crate) const MODEL_DIR: &str = "6";
/// The database file Grype reads (`v6.VulnerabilityDBFileName`).
pub(crate) const DB_FILE: &str = "vulnerability.db";
/// The file Grype writes beside it at import, with the database's digest (`v6.ImportMetadataFileName`).
pub(crate) const IMPORT_FILE: &str = "import.json";
/// Where an import writes its database inside its work directory: Grype's cache directory there.
pub(crate) const WORK_DB: &str = "db";
/// Grype's temporary directory inside its work directory.
pub(crate) const WORK_TMP: &str = "tmp";

const STAGING: &str = "staging";
const DBS: &str = "dbs";
const STATE: &str = "state.json";
const LOCK: &str = "lock";

/// The database runners mount, as the store published it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Held {
    /// Its directory under `dbs/`: `<built>-<first 12 hex digits of the archive's SHA-256>`.
    pub(crate) generation: String,
    /// When Anchore built it, in seconds since the epoch.
    pub(crate) built_at: u64,
    /// Its schema, such as `v6.1.10`.
    pub(crate) schema: String,
    /// The bytes of its directory.
    pub(crate) bytes: u64,
    /// The SHA-256 of its `vulnerability.db` when it was published: what a restart checks it against.
    pub(crate) sha256: String,
}

/// What `check` found of the held database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Checked {
    /// None is held.
    Empty,
    /// It is as it was published.
    Good,
    /// It changed or is gone: it was dropped, and the next update downloads one again.
    Dropped(String),
}

/// A database an import left in a work directory, to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Publishing {
    /// When Anchore built it, as Grype's check of it read.
    pub(crate) built_at: u64,
    /// Its schema, as Grype's check of it read.
    pub(crate) schema: String,
    /// The archive's file name in the work directory, removed before it is published.
    pub(crate) archive: String,
    /// The archive's SHA-256, which names its generation.
    pub(crate) archive_sha256: String,
}

/// The store, open: its lock held until it is dropped.
#[derive(Debug)]
pub(crate) struct GrypeStore {
    root: PathBuf,
    /// Held exclusively: a second runnerd on the same directory cannot open it.
    _lock: File,
    held: Option<Held>,
}

/// `state.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    held: Option<Held>,
}

impl GrypeStore {
    /// Opens the store at `root`, making it if missing: takes its lock, empties `staging/` and reads `state.json`.
    /// Fails with `WouldBlock` while another runnerd holds it.
    pub(crate) fn open(root: &Path) -> io::Result<GrypeStore> {
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
        let staging = root.join(STAGING);
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        make_dir(&staging, 0o700)?;
        make_dir(&root.join(DBS), 0o755)?;
        let state = match fs::read(root.join(STATE)) {
            Ok(bytes) => serde_json::from_slice::<Saved>(&bytes).unwrap_or_default(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Saved::default(),
            Err(error) => return Err(error),
        };
        let held = state.held.filter(|held| is_generation(&held.generation));
        Ok(GrypeStore {
            root: root.to_owned(),
            _lock: lock,
            held,
        })
    }

    /// The database runners mount, if one is held.
    pub(crate) fn held(&self) -> Option<&Held> {
        self.held.as_ref()
    }

    /// The directory a runner mounts for `generation`: Grype's cache directory, holding `6/`.
    pub(crate) fn generation_dir(&self, generation: &str) -> PathBuf {
        self.root.join(DBS).join(generation)
    }

    /// Hashes the held database against what was published, links never followed. One that differs, or is not
    /// there whole, is dropped: its directory removed and `state.json` written without it.
    pub(crate) fn check(&mut self) -> io::Result<Checked> {
        let Some(held) = self.held.clone() else {
            return Ok(Checked::Empty);
        };
        let dir = self.generation_dir(&held.generation).join(MODEL_DIR);
        let problem = match regular(&dir.join(IMPORT_FILE)).and_then(|()| {
            regular(&dir.join(DB_FILE))?;
            hash_file(&dir.join(DB_FILE))
        }) {
            Ok(sha256) if sha256 == held.sha256 => None,
            Ok(_) => Some("its database changed on disk".to_owned()),
            Err(error) => Some(format!("its database cannot be read: {error}")),
        };
        let Some(problem) = problem else {
            return Ok(Checked::Good);
        };
        self.save(None)?;
        self.held = None;
        remove_dir(&self.generation_dir(&held.generation))?;
        Ok(Checked::Dropped(problem))
    }

    /// A new, empty work directory under `staging/` for one download and import, with Grype's cache and temporary
    /// directories inside.
    pub(crate) fn work(&self) -> io::Result<PathBuf> {
        let staging = self.root.join(STAGING);
        for n in 0..u32::MAX {
            let work = staging.join(format!("work-{n}"));
            match fs::DirBuilder::new().mode(0o700).create(&work) {
                Ok(()) => {
                    make_dir(&work.join(WORK_DB), 0o700)?;
                    make_dir(&work.join(WORK_TMP), 0o700)?;
                    return Ok(work);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("no free work directory"))
    }

    /// Removes a work directory whose download or import failed.
    pub(crate) fn discard(&self, work: &Path) -> io::Result<()> {
        if work.starts_with(self.root.join(STAGING)) {
            remove_dir(work)
        } else {
            Ok(())
        }
    }

    /// Publishes the database Grype imported into `work`'s cache directory and checked: makes every directory 0755 and
    /// every file 0644, so a runner's user reads it, syncs it, moves it to `dbs/<generation>` and names it in
    /// `state.json`. Fails, leaving the held database as it was, if the import left a link or anything but
    /// directories and regular files, or no database and digest.
    pub(crate) fn publish(&mut self, work: &Path, imported: &Publishing) -> io::Result<Held> {
        // The archive is not part of the database.
        remove_dir(&work.join(&imported.archive))?;
        let db = work.join(WORK_DB);
        let mut bytes = 0;
        settle(&db, &mut bytes)?;
        let model = db.join(MODEL_DIR);
        regular(&model.join(IMPORT_FILE))?;
        regular(&model.join(DB_FILE))?;
        let sha256 = hash_file(&model.join(DB_FILE))?;
        let generation = format!(
            "{}-{}",
            imported.built_at,
            &imported.archive_sha256[..imported.archive_sha256.len().min(12)]
        );
        if !is_generation(&generation) {
            return Err(io::Error::other(format!(
                "{generation} is not a generation"
            )));
        }
        let target = self.generation_dir(&generation);
        if self
            .held
            .as_ref()
            .is_some_and(|held| held.generation == generation)
        {
            return Err(io::Error::other(format!(
                "{generation} is the database held"
            )));
        }
        // What a stop between a rename and the state's write left: no runner was given it.
        remove_dir(&target)?;
        fs::rename(&db, &target)?;
        let held = Held {
            generation,
            built_at: imported.built_at,
            schema: imported.schema.clone(),
            bytes,
            sha256,
        };
        // Held only once `state.json` names it: a restart then finds what was served.
        if let Err(error) = sync_dir(&self.root.join(DBS)).and_then(|()| self.save(Some(&held))) {
            let _ = remove_dir(&target);
            return Err(error);
        }
        self.held = Some(held.clone());
        self.discard(work)?;
        Ok(held)
    }

    /// Removes every generation under `dbs/` but the held one and those `mounted`, as containers' labels name them,
    /// and anything there that is not a generation. Returns the generations it removed.
    pub(crate) fn prune(&self, mounted: &BTreeSet<String>) -> io::Result<Vec<String>> {
        let mut removed = Vec::new();
        let held = self.held.as_ref().map(|held| held.generation.as_str());
        for entry in fs::read_dir(self.root.join(DBS))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if Some(name.as_str()) == held || mounted.contains(&name) {
                continue;
            }
            remove_dir(&entry.path())?;
            removed.push(name);
        }
        removed.sort();
        Ok(removed)
    }

    /// Writes `state.json` naming `held`: to a temporary file, synced, renamed over the last, the directory synced.
    fn save(&self, held: Option<&Held>) -> io::Result<()> {
        let state = Saved {
            held: held.cloned(),
        };
        let bytes = serde_json::to_vec_pretty(&state).map_err(io::Error::other)?;
        let temporary = self.root.join(format!("{STATE}.tmp"));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, self.root.join(STATE))?;
        sync_dir(&self.root)
    }
}

/// Whether `name` is a generation's: `<seconds>-<12 lowercase hex digits>`.
pub(crate) fn is_generation(name: &str) -> bool {
    name.split_once('-').is_some_and(|(built, sha)| {
        !built.is_empty()
            && built.len() <= 20
            && built.bytes().all(|b| b.is_ascii_digit())
            && sha.len() == 12
            && sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The lowercase hex SHA-256 of the file at `path`.
pub(crate) fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(hex(context.finish().as_ref()))
}

/// `bytes` as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// Makes every directory under `dir` 0755 and every file 0644, syncs each, and adds the files' bytes to `bytes`.
/// Fails on a link or anything but a directory or a regular file.
fn settle(dir: &Path, bytes: &mut u64) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            settle(&path, bytes)?;
        } else if kind.is_file() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
            File::open(&path)?.sync_all()?;
            *bytes += entry.metadata()?.len();
        } else {
            return Err(io::Error::other(format!(
                "the import left {}, which is not a file or a directory",
                path.display()
            )));
        }
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    sync_dir(dir)
}

/// Fails unless `path` is a regular file, links not followed.
fn regular(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_file() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )))
    }
}

/// Removes `dir` and everything in it, or the file there; nothing when nothing is there.
fn remove_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(dir),
        Ok(_) => fs::remove_file(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Makes `dir` with `mode` whatever the umask, if it is not there.
fn make_dir(dir: &Path, mode: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(mode).create(dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(mode))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
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
                "another runnerd keeps its Grype database here",
            )
        } else {
            error.into()
        }
    })
}
