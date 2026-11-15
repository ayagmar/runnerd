//! Whether what runners wrote without syncing may be lost to a host crash (ADR 0141): the one value that says when
//! the slots' volumes and the shared pnpm and Maven stores must be emptied before the next runner starts. It owns the
//! file that keeps that value across restarts and the kernel's boot ID it compares; it never calls Docker.

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Writes what the kernel holds in memory to disk: `sync(2)` on the host, a counter in tests.
#[derive(Clone)]
pub struct Flush(Arc<dyn Fn() + Send + Sync>);

impl Flush {
    /// A flush that calls `flush`.
    pub fn new(flush: impl Fn() + Send + Sync + 'static) -> Flush {
        Flush(Arc::new(flush))
    }

    /// The host's `sync(2)`, which every user may call.
    pub fn host() -> Flush {
        Flush::new(rustix::fs::sync)
    }
}

impl fmt::Debug for Flush {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Flush")
    }
}

/// What runners that skip their syncs may have left unwritten. Kept in a file of the daemon's, written before it
/// changes, so it survives a restart and a host crash; a reload does not change it, as a runner started before keeps
/// what it wrote whatever `skip_syncs` says now.
#[derive(Debug)]
pub(crate) struct Unsynced {
    /// Where the state is kept: absent while `Synced`.
    file: PathBuf,
    /// The kernel's boot ID, which changes with every boot: `/proc/sys/kernel/random/boot_id`.
    boot_id: PathBuf,
    flush: Flush,
    state: Owed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Owed {
    /// Everything runners wrote is on disk, or none skipped its syncs since it was.
    Synced,
    /// A runner that skips its syncs started during the boot named; what it wrote may be only in memory.
    Since { boot: String },
    /// The host booted again after `Since` with no clean stop between: what runners wrote may be torn, so the slots'
    /// volumes and the shared stores are emptied before any runner starts.
    Torn,
}

/// What a file that is not one this module writes reads as: what it held cannot be told, so it is kept as torn.
const TORN: &str = "torn";
const SINCE: &str = "since ";

impl Unsynced {
    /// The state `file` keeps. A file that cannot be read, or holds what this module never writes, reads as torn:
    /// emptying the caches once costs less than a job on a torn one.
    pub(crate) fn open(file: PathBuf, proc: &Path, flush: Flush) -> Unsynced {
        let state = match std::fs::read_to_string(&file) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Owed::Synced,
            Err(_) => Owed::Torn,
            Ok(text) => match text.trim_end().strip_prefix(SINCE) {
                Some(boot) if !boot.is_empty() => Owed::Since {
                    boot: boot.to_owned(),
                },
                _ => Owed::Torn,
            },
        };
        Unsynced {
            file,
            boot_id: proc.join("sys/kernel/random/boot_id"),
            flush,
            state,
        }
    }

    fn boot(&self) -> Option<String> {
        let boot = std::fs::read_to_string(&self.boot_id).ok()?;
        let boot = boot.trim();
        (!boot.is_empty()).then(|| boot.to_owned())
    }

    /// At start: a host that booted since runners skipped their syncs, with no clean stop between, may have lost what
    /// they wrote. Returns whether that made the caches torn.
    pub(crate) fn recovered(&mut self) -> io::Result<bool> {
        let Owed::Since { boot } = &self.state else {
            return Ok(false);
        };
        if self.boot().as_deref() == Some(boot.as_str()) {
            return Ok(false);
        }
        self.state = Owed::Torn;
        write(&self.file, TORN)?;
        Ok(true)
    }

    /// Whether the caches must be emptied before any runner starts.
    pub(crate) fn is_torn(&self) -> bool {
        self.state == Owed::Torn
    }

    /// Before a runner that may skip its syncs starts: whether it may, once that is kept on disk. A runner that may not
    /// keeps its syncs: while the caches are torn none starts, and where the boot ID or the file cannot be had, a
    /// later crash could not be told.
    pub(crate) fn runner_skips(&mut self) -> bool {
        let Some(boot) = self.boot() else {
            return false;
        };
        match &self.state {
            Owed::Torn => false,
            Owed::Since { boot: since } if *since == boot => true,
            Owed::Synced | Owed::Since { .. } => {
                if write(&self.file, &format!("{SINCE}{boot}")).is_err() {
                    return false;
                }
                self.state = Owed::Since { boot };
                true
            }
        }
    }

    /// The torn caches are emptied: once that is on disk, nothing is owed.
    pub(crate) fn emptied(&mut self) -> io::Result<()> {
        if self.state != Owed::Torn {
            return Ok(());
        }
        self.settle()
    }

    /// A stop that leaves no runner running: once what runners wrote is on disk, nothing is owed. Torn caches stay
    /// owed: they are emptied at the next start.
    pub(crate) fn stopped_clean(&mut self) -> io::Result<()> {
        if !matches!(self.state, Owed::Since { .. }) {
            return Ok(());
        }
        self.settle()
    }

    fn settle(&mut self) -> io::Result<()> {
        (self.flush.0)();
        match std::fs::remove_file(&self.file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        sync_parent(&self.file)?;
        self.state = Owed::Synced;
        Ok(())
    }
}

/// Replaces `file` with `text`, on disk before it returns: a temporary file synced, renamed over it, and the directory
/// synced.
fn write(file: &Path, text: &str) -> io::Result<()> {
    let temporary = file.with_extension("new");
    {
        let mut out = std::fs::File::create(&temporary)?;
        out.write_all(text.as_bytes())?;
        out.write_all(b"\n")?;
        out.sync_all()?;
    }
    std::fs::rename(&temporary, file)?;
    sync_parent(file)
}

fn sync_parent(file: &Path) -> io::Result<()> {
    match file.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => std::fs::File::open(dir)?.sync_all(),
        _ => Ok(()),
    }
}
