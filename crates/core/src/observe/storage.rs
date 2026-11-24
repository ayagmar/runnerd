//! What file systems hold the job history's database and Docker's root, and how they write (ADR 0171): whether btrfs
//! copies the database on write, which fragments a file SQLite rewrites a page at a time and multiplies what reaches
//! the drive, and whether Docker's root is btrfs, another file system, or one in a file on a loop device. It words
//! the doctor's checks of both, skipped or not, which every client shows as they are. Reads `statfs`, inode flags and,
//! through `disk`, the mounts and devices under Docker's root once its links are resolved; the one change it makes is
//! turning copy-on-write off for a directory or an empty file the store has just created.

use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::IFlags;

use super::disk::Disks;
use super::metrics::Paths;
use crate::api::{Check, CheckStatus};
use crate::config::ByteSize;

/// A file system, by the magic number `statfs` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystem {
    /// btrfs, which copies a file's extents on write unless the file has `FS_NOCOW_FL`.
    Btrfs,
    /// ext2, ext3 or ext4, which share one magic number.
    Ext4,
    /// XFS.
    Xfs,
    /// tmpfs: memory, nothing reaches a drive.
    Tmpfs,
    /// `OpenZFS`, which copies on write whatever the file's flags.
    Zfs,
    /// Another, by its magic number.
    Other(u64),
}

impl FileSystem {
    /// The one under `path`.
    pub fn of(path: &Path) -> io::Result<FileSystem> {
        let found = rustix::fs::statfs(path)?;
        Ok(FileSystem::from_magic(
            u64::try_from(found.f_type).unwrap_or_default(),
        ))
    }

    /// The one whose `statfs` magic number is `magic`.
    pub fn from_magic(magic: u64) -> FileSystem {
        match magic {
            0x9123_683e => FileSystem::Btrfs,
            0xef53 => FileSystem::Ext4,
            0x5846_5342 => FileSystem::Xfs,
            0x0102_1994 => FileSystem::Tmpfs,
            0x2fc1_2fc1 => FileSystem::Zfs,
            other => FileSystem::Other(other),
        }
    }

    /// Its name as `mount` shows it; another by its magic number, such as `0x794c7630`.
    pub fn name(self) -> String {
        match self {
            FileSystem::Btrfs => "btrfs".to_owned(),
            FileSystem::Ext4 => "ext4".to_owned(),
            FileSystem::Xfs => "xfs".to_owned(),
            FileSystem::Tmpfs => "tmpfs".to_owned(),
            FileSystem::Zfs => "zfs".to_owned(),
            FileSystem::Other(magic) => format!("a file system of magic {magic:#x}"),
        }
    }
}

/// Turns copy-on-write off for `path`, a directory or an empty file just created, when it is on btrfs: files a
/// directory holds later take the flag from it, and an empty file keeps it as it grows. `true` when it is off now;
/// `false` on another file system, which is left as it is.
pub fn no_cow(path: &Path) -> io::Result<bool> {
    if FileSystem::of(path)? != FileSystem::Btrfs {
        return Ok(false);
    }
    let file = std::fs::File::open(path)?;
    let flags = rustix::fs::ioctl_getflags(&file)?;
    if !flags.contains(IFlags::NOCOW) {
        rustix::fs::ioctl_setflags(&file, flags | IFlags::NOCOW)?;
    }
    Ok(rustix::fs::ioctl_getflags(&file)?.contains(IFlags::NOCOW))
}

/// Whether btrfs copies `path` on write: `false` once it has `FS_NOCOW_FL`.
fn copied_on_write(path: &Path) -> io::Result<bool> {
    let file = std::fs::File::open(path)?;
    Ok(!rustix::fs::ioctl_getflags(&file)?.contains(IFlags::NOCOW))
}

/// The history's database as the doctor finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// The database file.
    pub path: PathBuf,
    /// What is there.
    pub found: HistoryFound,
}

/// What the doctor finds at the history's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryFound {
    /// No database yet: the daemon's first start creates it.
    Missing,
    /// The database, its size and its file system; on btrfs, whether it is copied on write.
    Present {
        /// The file's size.
        bytes: u64,
        /// The file system it is on.
        file_system: FileSystem,
        /// On btrfs, whether the file lacks `FS_NOCOW_FL`; `None` elsewhere or when its flags cannot be read.
        copied: Option<bool>,
    },
    /// It could not be read, with why.
    Unread(String),
}

impl History {
    /// The database at `path`, as it is now.
    pub fn read(path: &Path) -> History {
        let found = match std::fs::metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => HistoryFound::Missing,
            Err(error) => HistoryFound::Unread(error.to_string()),
            Ok(metadata) => match FileSystem::of(path) {
                Err(error) => HistoryFound::Unread(error.to_string()),
                Ok(file_system) => HistoryFound::Present {
                    bytes: metadata.len(),
                    file_system,
                    copied: (file_system == FileSystem::Btrfs)
                        .then(|| copied_on_write(path).ok())
                        .flatten(),
                },
            },
        };
        History {
            path: path.to_owned(),
            found,
        }
    }

    /// The doctor's check of it: a warning while btrfs copies it on write, with how to make a copy that is not.
    pub fn check(&self) -> Check {
        const NAME: &str = "history storage";
        let path = self.path.display();
        match &self.found {
            HistoryFound::Missing => check(
                NAME,
                CheckStatus::Info,
                format!(
                    "{path} does not exist yet; the daemon creates it, without copy-on-write on btrfs"
                ),
                None,
            ),
            HistoryFound::Unread(why) => check(
                NAME,
                CheckStatus::Info,
                format!("cannot read {path}: {why}"),
                None,
            ),
            HistoryFound::Present {
                bytes,
                file_system: FileSystem::Btrfs,
                copied: Some(true),
            } => check(
                NAME,
                CheckStatus::Warn,
                format!(
                    "{path}, {}, is on btrfs with copy-on-write: each page SQLite writes copies its extent, which \
                     fragments the file and multiplies what the drive writes",
                    ByteSize(*bytes)
                ),
                Some(nocow_fix(&self.path)),
            ),
            HistoryFound::Present {
                bytes,
                file_system: FileSystem::Btrfs,
                copied: Some(false),
            } => check(
                NAME,
                CheckStatus::Pass,
                format!(
                    "{path}, {}, on btrfs without copy-on-write",
                    ByteSize(*bytes)
                ),
                None,
            ),
            HistoryFound::Present {
                bytes,
                file_system,
                copied: _,
            } => check(
                NAME,
                if *file_system == FileSystem::Btrfs {
                    CheckStatus::Info
                } else {
                    CheckStatus::Pass
                },
                format!(
                    "{path}, {}, on {}{}",
                    ByteSize(*bytes),
                    file_system.name(),
                    if *file_system == FileSystem::Btrfs {
                        "; whether it is copied on write cannot be read"
                    } else {
                        ""
                    }
                ),
                None,
            ),
        }
    }
}

/// How to replace the database at `path` with a copy btrfs does not copy on write: a file takes the flag only while
/// empty, so the copy is made in its directory once that has the flag. `VACUUM INTO` copies what the `-wal` holds
/// too, and leaves out the pages pruning freed. Each step runs only if the one before succeeded, in a subshell with
/// umask 077: the copy goes into a new empty owner-only file `mktemp` makes, which `VACUUM INTO` takes and a stale
/// copy never is; it replaces the database only once SQLite finds it whole and its mode is 600, and the database it
/// replaces, with its `-wal`, is kept beside it as `.cow` until its owner removes it.
fn nocow_fix(path: &Path) -> String {
    let dir = shell_word(
        &path
            .parent()
            .unwrap_or(Path::new("."))
            .display()
            .to_string(),
    );
    let name = path
        .file_name()
        .map_or_else(|| "runnerd.db".into(), |name| name.to_string_lossy());
    let (db, wal, shm, cow, cow_wal) = (
        shell_word(&name),
        shell_word(&format!("{name}-wal")),
        shell_word(&format!("{name}-shm")),
        shell_word(&format!("{name}.cow")),
        shell_word(&format!("{name}.cow-wal")),
    );
    format!(
        "with runnerd stopped (runnerd stop, then wait for systemctl --user is-active runnerd to say inactive), in \
         one shell:\n\
         (umask 077 && cd {dir} && test ! -e {cow} && chattr +C . &&\n\
         copy=$(mktemp runnerd-nocow.XXXXXX) &&\n\
         sqlite3 {db} \"VACUUM INTO '$copy'\" &&\n\
         test \"$(sqlite3 \"$copy\" 'PRAGMA integrity_check')\" = ok &&\n\
         test \"$(stat -c %a \"$copy\")\" = 600 &&\n\
         mv {db} {cow} && {{ test ! -e {wal} || mv {wal} {cow_wal}; }} && rm -f {shm} &&\n\
         mv \"$copy\" {db}) &&\n\
         systemctl --user start runnerd\n\
         then, once runnerd runs as before, remove {cow} in {dir}"
    )
}

/// `word` as one shell word: as it is when it needs no quoting, else in single quotes.
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-+:,@%".contains(&byte))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Docker's root directory as the doctor finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerRoot {
    /// Docker's root as Docker names it, such as `/var/lib/docker`.
    pub path: PathBuf,
    /// The file system that holds it, or why it cannot be read.
    pub file_system: Result<FileSystem, String>,
    /// What that file system is on.
    pub backing: Backing,
}

/// What the file system that holds Docker's root is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    /// A device: a disk or a partition.
    Device,
    /// A file, on these loop devices, joined by `+` (`disk::Disks::store`).
    Loops(String),
    /// It cannot be read: its mount, a device or a loop device's backing file is not found.
    Unresolved,
}

/// Why the doctor has no Docker root to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSkipped {
    /// Docker did not answer.
    NoDocker,
    /// Docker answered without naming its root.
    Unnamed,
}

impl DockerRoot {
    /// Docker's root, `root`, resolved once, links and all, so its file system and its mount are read of the same
    /// directory: `paths`' `mountinfo` names mount points without links.
    pub fn read(paths: &Paths, root: &Path) -> DockerRoot {
        let resolved = match std::fs::canonicalize(root) {
            Ok(resolved) => resolved,
            Err(error) => {
                return DockerRoot {
                    path: root.to_owned(),
                    file_system: Err(error.to_string()),
                    backing: Backing::Unresolved,
                };
            }
        };
        DockerRoot {
            path: root.to_owned(),
            file_system: FileSystem::of(&resolved).map_err(|error| error.to_string()),
            backing: match Disks::resolve(paths, &resolved) {
                None => Backing::Unresolved,
                Some(disks) => disks.store().map_or(Backing::Device, Backing::Loops),
            },
        }
    }

    /// The check when there is no root to read, skipped for `why`.
    pub fn skipped(why: RootSkipped) -> Check {
        check(
            DOCKER_STORAGE,
            CheckStatus::Skip,
            match why {
                RootSkipped::NoDocker => "needs Docker",
                RootSkipped::Unnamed => "Docker does not say where its root is",
            }
            .to_owned(),
            None,
        )
    }

    /// The doctor's check of it: which file system takes Docker's writes, and what writes less than btrfs.
    pub fn check(&self) -> Check {
        let path = self.path.display();
        let file_system = match &self.file_system {
            Ok(file_system) => *file_system,
            Err(why) => {
                return check(
                    DOCKER_STORAGE,
                    CheckStatus::Info,
                    format!("cannot read the file system of {path}: {why}"),
                    None,
                );
            }
        };
        if let Backing::Loops(loops) = &self.backing {
            return check(
                DOCKER_STORAGE,
                CheckStatus::Pass,
                format!(
                    "{path} is {} in a file on {loops}: keep that file without copy-on-write if it is on btrfs \
                     (chattr +C on it while empty)",
                    file_system.name()
                ),
                None,
            );
        }
        match file_system {
            FileSystem::Btrfs => check(
                DOCKER_STORAGE,
                CheckStatus::Info,
                format!(
                    "{path} is on btrfs: the layers, volumes and caches runners write are copied on write and their \
                     metadata written twice, which multiplies what the drive writes"
                ),
                Some(format!(
                    "a file system of its own at {path} writes less: an ext4 or XFS partition, or an ext4 file \
                     system in a file made without copy-on-write (chattr +C), mounted there while Docker is stopped"
                )),
            ),
            FileSystem::Ext4 | FileSystem::Xfs if self.backing == Backing::Device => check(
                DOCKER_STORAGE,
                CheckStatus::Pass,
                format!("{path} is on {}", file_system.name()),
                None,
            ),
            FileSystem::Ext4 | FileSystem::Xfs => check(
                DOCKER_STORAGE,
                CheckStatus::Info,
                format!(
                    "{path} is on {}; whether it is in a file on a loop device cannot be read",
                    file_system.name()
                ),
                None,
            ),
            FileSystem::Tmpfs | FileSystem::Zfs | FileSystem::Other(_) => check(
                DOCKER_STORAGE,
                CheckStatus::Info,
                format!("{path} is on {}", file_system.name()),
                None,
            ),
        }
    }
}

/// The name of the check of Docker's root.
const DOCKER_STORAGE: &str = "Docker storage";

fn check(name: &str, status: CheckStatus, detail: String, fix: Option<String>) -> Check {
    Check {
        name: name.to_owned(),
        status,
        detail,
        fix,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Backing, DockerRoot, FileSystem, History, HistoryFound, RootSkipped, no_cow};
    use crate::api::CheckStatus;
    use crate::testdir::TestDir;

    fn history(found: HistoryFound) -> History {
        History {
            path: PathBuf::from("/home/me/.local/state/runnerd/runnerd.db"),
            found,
        }
    }

    #[test]
    fn a_history_btrfs_copies_on_write_warns_with_a_copy_that_is_not() {
        // Catches the warning on a database already without copy-on-write, or a fix that copies before the flag is
        // set, or misses the -wal.
        let cow = history(HistoryFound::Present {
            bytes: 5_852_561_408,
            file_system: FileSystem::Btrfs,
            copied: Some(true),
        })
        .check();
        assert_eq!(cow.status, CheckStatus::Warn);
        assert_eq!(
            cow.detail,
            "/home/me/.local/state/runnerd/runnerd.db, 5.45G, is on btrfs with copy-on-write: each page SQLite \
             writes copies its extent, which fragments the file and multiplies what the drive writes"
        );
        assert_eq!(
            cow.fix.as_deref(),
            Some(
                "with runnerd stopped (runnerd stop, then wait for systemctl --user is-active runnerd to say \
                 inactive), in one shell:\n\
                 (umask 077 && cd /home/me/.local/state/runnerd && test ! -e runnerd.db.cow && chattr +C . &&\n\
                 copy=$(mktemp runnerd-nocow.XXXXXX) &&\n\
                 sqlite3 runnerd.db \"VACUUM INTO '$copy'\" &&\n\
                 test \"$(sqlite3 \"$copy\" 'PRAGMA integrity_check')\" = ok &&\n\
                 test \"$(stat -c %a \"$copy\")\" = 600 &&\n\
                 mv runnerd.db runnerd.db.cow && { test ! -e runnerd.db-wal || mv runnerd.db-wal runnerd.db.cow-wal; } \
                 && rm -f runnerd.db-shm &&\n\
                 mv \"$copy\" runnerd.db) &&\n\
                 systemctl --user start runnerd\n\
                 then, once runnerd runs as before, remove runnerd.db.cow in /home/me/.local/state/runnerd"
            )
        );
        let nocow = history(HistoryFound::Present {
            bytes: 1 << 30,
            file_system: FileSystem::Btrfs,
            copied: Some(false),
        })
        .check();
        assert_eq!(
            (nocow.status, nocow.detail.as_str(), nocow.fix),
            (
                CheckStatus::Pass,
                "/home/me/.local/state/runnerd/runnerd.db, 1G, on btrfs without copy-on-write",
                None
            )
        );
        let ext4 = history(HistoryFound::Present {
            bytes: 1 << 20,
            file_system: FileSystem::Ext4,
            copied: None,
        })
        .check();
        assert_eq!(
            (ext4.status, ext4.detail.as_str()),
            (
                CheckStatus::Pass,
                "/home/me/.local/state/runnerd/runnerd.db, 1M, on ext4"
            )
        );
        let unknown = history(HistoryFound::Present {
            bytes: 1 << 20,
            file_system: FileSystem::Btrfs,
            copied: None,
        })
        .check();
        assert_eq!(
            unknown.status,
            CheckStatus::Info,
            "unread flags do not warn"
        );
        assert_eq!(
            history(HistoryFound::Missing).check().status,
            CheckStatus::Info
        );
    }

    /// What following `fix` did in `dir`, with `sqlite3`, `chattr` and `systemctl` stubbed: `sqlite3` copies the
    /// database for `VACUUM INTO`, refusing a destination that is not empty as SQLite does, or fails when `fail`.
    /// Whether it started runnerd.
    fn follow(fix: &str, dir: &std::path::Path, fail: bool) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sqlite3 = format!(
            "#!/bin/sh\n\
             case \"$2\" in\n\
             \"VACUUM INTO '\"*) dest=${{2#\"VACUUM INTO '\"}}; dest=${{dest%\"'\"}}\n\
             if [ -s \"$dest\" ] || {fail}; then exit 1; fi; cat \"$1\" > \"$dest\";;\n\
             'PRAGMA integrity_check') echo ok;;\n\
             esac\n"
        );
        for (name, body) in [
            ("sqlite3", sqlite3.as_str()),
            ("chattr", "#!/bin/sh\n"),
            ("systemctl", "#!/bin/sh\ntouch \"$STARTED\"\n"),
        ] {
            std::fs::write(bin.join(name), body).unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let script: Vec<&str> = fix
            .lines()
            .skip(1)
            .take_while(|line| !line.starts_with("then,"))
            .collect();
        let started = dir.join("started");
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("umask 022\n{}", script.join("\n")))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("STARTED", &started)
            .status()
            .unwrap();
        started.exists()
    }

    #[test]
    fn the_history_fix_installs_only_a_fresh_owner_only_copy_and_keeps_the_original() {
        // Catches a fix that installs a stale copy a past attempt left after `VACUUM INTO` refused to overwrite it, a
        // copy readable by others under umask 022, and an original removed before the copy is checked.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TestDir::new("storage");
        let state = dir.0.join("state dir");
        std::fs::create_dir_all(&state).unwrap();
        let db = state.join("runnerd.db");
        let fix = history(HistoryFound::Present {
            bytes: 1,
            file_system: FileSystem::Btrfs,
            copied: Some(true),
        });
        let fix = History {
            path: db.clone(),
            ..fix
        }
        .check()
        .fix
        .unwrap();
        let reset = || {
            for entry in std::fs::read_dir(&state).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
            std::fs::write(&db, "current").unwrap();
            std::fs::write(state.join("runnerd.db-wal"), "wal").unwrap();
            // What earlier attempts left: a full copy, under the old name and a name mktemp could give.
            std::fs::write(state.join("runnerd.db.nocow"), "stale").unwrap();
            std::fs::write(state.join("runnerd-nocow.AAAAAA"), "stale").unwrap();
        };
        let read = |name: &str| std::fs::read_to_string(state.join(name)).ok();

        reset();
        assert!(
            !follow(&fix, &dir.0, true),
            "a failed vacuum starts nothing"
        );
        assert_eq!(read("runnerd.db").as_deref(), Some("current"));
        assert_eq!(read("runnerd.db-wal").as_deref(), Some("wal"));
        assert_eq!(read("runnerd.db.cow"), None);

        reset();
        assert!(follow(&fix, &dir.0, false), "a checked copy starts runnerd");
        assert_eq!(
            read("runnerd.db").as_deref(),
            Some("current"),
            "the fresh copy, not a stale one"
        );
        let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "owner only under umask 022");
        assert_eq!(
            read("runnerd.db.cow").as_deref(),
            Some("current"),
            "the original is kept"
        );
        assert_eq!(
            read("runnerd.db.cow-wal").as_deref(),
            Some("wal"),
            "with its -wal"
        );
        assert_eq!(read("runnerd.db-wal"), None);
        std::fs::remove_file(dir.0.join("started")).unwrap();

        // Again, with the original of the first still kept: nothing is replaced.
        std::fs::write(&db, "newer").unwrap();
        assert!(!follow(&fix, &dir.0, false), "a kept original stops it");
        assert_eq!(read("runnerd.db").as_deref(), Some("newer"));
        assert_eq!(read("runnerd.db.cow").as_deref(), Some("current"));
    }

    #[test]
    fn docker_storage_tells_btrfs_from_ext4_and_xfs_and_a_file_on_a_loop_device() {
        // Catches a loop-backed ext4 root read as btrfs's, or btrfs advice shown for a root that writes no more.
        let root = |file_system, loops: Option<&str>| {
            DockerRoot {
                path: PathBuf::from("/var/lib/docker"),
                file_system,
                backing: loops.map_or(Backing::Device, |loops| Backing::Loops(loops.to_owned())),
            }
            .check()
        };
        let btrfs = root(Ok(FileSystem::Btrfs), None);
        assert_eq!(btrfs.status, CheckStatus::Info);
        assert!(
            btrfs.detail.starts_with("/var/lib/docker is on btrfs: "),
            "{}",
            btrfs.detail
        );
        assert!(btrfs.fix.is_some(), "btrfs says what writes less");
        for (file_system, said) in [
            (FileSystem::Ext4, "/var/lib/docker is on ext4"),
            (FileSystem::Xfs, "/var/lib/docker is on xfs"),
        ] {
            let check = root(Ok(file_system), None);
            assert_eq!(
                (check.status, check.detail.as_str(), check.fix),
                (CheckStatus::Pass, said, None)
            );
        }
        let looped = root(Ok(FileSystem::Ext4), Some("loop0"));
        assert_eq!(looped.status, CheckStatus::Pass);
        assert_eq!(
            looped.detail,
            "/var/lib/docker is ext4 in a file on loop0: keep that file without copy-on-write if it is on btrfs \
             (chattr +C on it while empty)"
        );
        let overlay = root(Ok(FileSystem::from_magic(0x794c_7630)), None);
        assert_eq!(
            (overlay.status, overlay.detail.as_str()),
            (
                CheckStatus::Info,
                "/var/lib/docker is on a file system of magic 0x794c7630"
            )
        );
        let unread = root(Err("permission denied".to_owned()), None);
        assert_eq!(unread.status, CheckStatus::Info);
        // A mount whose device or backing file is not found is not read as a device of its own.
        let unresolved = DockerRoot {
            path: PathBuf::from("/var/lib/docker"),
            file_system: Ok(FileSystem::Ext4),
            backing: Backing::Unresolved,
        }
        .check();
        assert_eq!(
            (unresolved.status, unresolved.detail.as_str()),
            (
                CheckStatus::Info,
                "/var/lib/docker is on ext4; whether it is in a file on a loop device cannot be read"
            )
        );
        assert_eq!(
            [RootSkipped::NoDocker, RootSkipped::Unnamed].map(|why| {
                let check = DockerRoot::skipped(why);
                (check.name, check.status, check.detail)
            }),
            [
                (
                    "Docker storage".to_owned(),
                    CheckStatus::Skip,
                    "needs Docker".to_owned()
                ),
                (
                    "Docker storage".to_owned(),
                    CheckStatus::Skip,
                    "Docker does not say where its root is".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_new_directory_loses_copy_on_write_only_on_btrfs_and_its_files_inherit_it() {
        // Catches a flag set on another file system, or one the directory's new database does not take.
        let dir = TestDir::new("storage");
        let on = FileSystem::of(&dir.0).unwrap();
        let set = no_cow(&dir.0).unwrap();
        assert_eq!(set, on == FileSystem::Btrfs, "on {on:?}");
        let file = dir.0.join("runnerd.db");
        std::fs::write(&file, b"").unwrap();
        let found = History::read(&file).found;
        match found {
            HistoryFound::Present {
                file_system: FileSystem::Btrfs,
                copied,
                ..
            } => assert_eq!(copied, Some(false), "a file made in it inherits the flag"),
            HistoryFound::Present { copied, .. } => assert_eq!(copied, None),
            other => panic!("{other:?}"),
        }
    }
}
