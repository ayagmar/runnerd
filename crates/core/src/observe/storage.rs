//! What file systems hold the job history's database and Docker's root, and how they write (ADR 0171): whether btrfs
//! copies the database on write, which fragments a file SQLite rewrites a page at a time and multiplies what reaches
//! the drive, and whether Docker's root is btrfs, another file system, or one in a file on a loop device. It words
//! the doctor's checks of both, which every client shows as they are. Reads `statfs` and inode flags; the one change
//! it makes is turning copy-on-write off for a directory or an empty file the store has just created.

use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::IFlags;

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
/// too, and leaves out the pages pruning freed.
fn nocow_fix(path: &Path) -> String {
    let dir = path.parent().unwrap_or(Path::new("."));
    let copy = {
        let mut copy = path.as_os_str().to_owned();
        copy.push(".nocow");
        PathBuf::from(copy)
    };
    let (db, copy_word, dir) = (
        shell_word(&path.display().to_string()),
        shell_word(&copy.display().to_string()),
        shell_word(&dir.display().to_string()),
    );
    let copy_text = copy.display().to_string();
    let sql = if shell_word(&copy_text) == copy_text {
        format!("\"VACUUM INTO '{copy_text}'\"")
    } else {
        shell_word(&format!("VACUUM INTO '{}'", copy_text.replace('\'', "''")))
    };
    format!(
        "with runnerd stopped (runnerd stop, then wait for systemctl --user is-active runnerd to say inactive):\n\
         chattr +C {dir}\n\
         sqlite3 {db} {sql}\n\
         rm -f {db}-wal {db}-shm && mv {copy_word} {db}\n\
         systemctl --user start runnerd"
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
    /// Docker's root, such as `/var/lib/docker`.
    pub path: PathBuf,
    /// Its file system, or why it cannot be read.
    pub file_system: Result<FileSystem, String>,
    /// The loop devices it is on, joined by `+`, when it is a file system in a file (`disk::Disks::store`).
    pub loops: Option<String>,
}

impl DockerRoot {
    /// The doctor's check of it: which file system takes Docker's writes, and what writes less than btrfs.
    pub fn check(&self) -> Check {
        const NAME: &str = "Docker storage";
        let path = self.path.display();
        let file_system = match &self.file_system {
            Ok(file_system) => *file_system,
            Err(why) => {
                return check(
                    NAME,
                    CheckStatus::Info,
                    format!("cannot read the file system of {path}: {why}"),
                    None,
                );
            }
        };
        if let Some(loops) = &self.loops {
            return check(
                NAME,
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
                NAME,
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
            FileSystem::Ext4 | FileSystem::Xfs => check(
                NAME,
                CheckStatus::Pass,
                format!("{path} is on {}", file_system.name()),
                None,
            ),
            FileSystem::Tmpfs | FileSystem::Zfs | FileSystem::Other(_) => check(
                NAME,
                CheckStatus::Info,
                format!("{path} is on {}", file_system.name()),
                None,
            ),
        }
    }
}

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

    use super::{DockerRoot, FileSystem, History, HistoryFound, no_cow};
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
                "with runnerd stopped (runnerd stop, then wait for systemctl --user is-active runnerd to say inactive):\n\
                 chattr +C /home/me/.local/state/runnerd\n\
                 sqlite3 /home/me/.local/state/runnerd/runnerd.db \
                 \"VACUUM INTO '/home/me/.local/state/runnerd/runnerd.db.nocow'\"\n\
                 rm -f /home/me/.local/state/runnerd/runnerd.db-wal /home/me/.local/state/runnerd/runnerd.db-shm \
                 && mv /home/me/.local/state/runnerd/runnerd.db.nocow /home/me/.local/state/runnerd/runnerd.db\n\
                 systemctl --user start runnerd"
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

    #[test]
    fn docker_storage_tells_btrfs_from_ext4_and_xfs_and_a_file_on_a_loop_device() {
        // Catches a loop-backed ext4 root read as btrfs's, or btrfs advice shown for a root that writes no more.
        let root = |file_system, loops: Option<&str>| {
            DockerRoot {
                path: PathBuf::from("/var/lib/docker"),
                file_system,
                loops: loops.map(str::to_owned),
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
