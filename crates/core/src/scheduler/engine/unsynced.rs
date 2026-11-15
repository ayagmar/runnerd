//! Whether what runners wrote without syncing may be lost to a host crash (ADR 0141): the one value that says when
//! the slots' volumes and the shared pnpm and Maven stores must be emptied before the next runner starts. It owns the
//! file that keeps that value across restarts and the kernel's boot ID it compares, and asks `Durability` whether a
//! past boot ended in an orderly shutdown; it never calls Docker.

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How the host makes what runners wrote durable, and tells whether it did: `sync(2)` and the systemd journal on the
/// host (`boots`), a counter and a script in tests.
#[derive(Clone)]
pub struct Durability(Arc<Hooks>);

/// What `Durability` calls, behind one pointer: the engine is held across every await of its loop.
struct Hooks {
    flush: Box<dyn Fn() + Send + Sync>,
    ended_synced: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl Durability {
    /// `flush` writes what the kernel holds in memory to disk; `ended_synced` says whether the boot of the boot ID it
    /// is given ended in an orderly shutdown whose final sync finished.
    pub fn new(
        flush: impl Fn() + Send + Sync + 'static,
        ended_synced: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Durability {
        Durability(Arc::new(Hooks {
            flush: Box::new(flush),
            ended_synced: Box::new(ended_synced),
        }))
    }

    /// The host's `sync(2)`, which every user may call, and its journal (`boots::ended_synced`).
    pub fn host() -> Durability {
        Durability::new(rustix::fs::sync, crate::boots::ended_synced)
    }
}

impl fmt::Debug for Durability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Durability")
    }
}

/// What runners that skip their syncs may have left unwritten. Kept in a file of the daemon's, written before it
/// changes, so it survives a restart and a host crash; a reload does not change it, as a runner started before keeps
/// what it wrote whatever `skip_syncs` says now. The count of refused emptyings is not kept: a restart tries
/// `REFUSALS` times again.
#[derive(Debug)]
pub(crate) struct Unsynced {
    /// Where the state is kept: absent while `Synced`.
    file: PathBuf,
    /// The kernel's boot ID, which changes with every boot: `/proc/sys/kernel/random/boot_id`.
    boot_id: PathBuf,
    durability: Durability,
    state: Owed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Owed {
    /// Everything runners wrote is on disk, or none skipped its syncs since it was.
    Synced,
    /// A runner that skips its syncs started during the boot named; what it wrote may be only in memory.
    Since { boot: String },
    /// The host booted again after `Since` with no clean stop between and no orderly shutdown that ended it: what
    /// runners wrote may be torn, so the slots' volumes and the shared stores are emptied before any runner starts.
    /// `refusals` counts the ticks since this daemon started that found a container left on a torn volume; `held`
    /// are the volumes the last of them could not empty.
    Torn { refusals: u32, held: Vec<String> },
}

/// How many emptyings Docker may refuse before the engine gives up on them and starts runners on what the caches
/// hold, rather than start none.
pub(crate) const REFUSALS: u32 = 5;

/// What `recovered` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovered {
    /// Nothing is owed, or the host has not booted since runners skipped their syncs.
    Unchanged,
    /// The host booted since, after an orderly shutdown that wrote everything: nothing is owed.
    ShutDownInOrder,
    /// The host booted since with neither a clean stop nor an orderly shutdown: the caches are torn.
    Torn,
}

/// What `refused` came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterRefusal {
    /// The caches stay torn, and the next tick tries again: `tries` of `REFUSALS` were refused.
    Again { tries: u32 },
    /// That was the last try: the engine may give up (`gave_up`) once no slot's layers it could not empty are left
    /// as they are. Until then the caches stay torn, on disk too, so a restart tries again.
    GiveUp,
}

/// What a file that is not one this module writes reads as: what it held cannot be told, so it is kept as torn.
const TORN: &str = "torn";
const SINCE: &str = "since ";

impl Unsynced {
    /// The state `file` keeps. A file that cannot be read, or holds what this module never writes, reads as torn:
    /// emptying the caches once costs less than a job on a torn one.
    pub(crate) fn open(file: PathBuf, proc: &Path, durability: Durability) -> Unsynced {
        let state = match std::fs::read_to_string(&file) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Owed::Synced,
            Err(_) => Owed::Torn {
                refusals: 0,
                held: Vec::new(),
            },
            Ok(text) => match text.trim_end().strip_prefix(SINCE) {
                Some(boot) if !boot.is_empty() => Owed::Since {
                    boot: boot.to_owned(),
                },
                _ => Owed::Torn {
                    refusals: 0,
                    held: Vec::new(),
                },
            },
        };
        Unsynced {
            file,
            boot_id: proc.join("sys/kernel/random/boot_id"),
            durability,
            state,
        }
    }

    fn boot(&self) -> Option<String> {
        let boot = std::fs::read_to_string(&self.boot_id).ok()?;
        let boot = boot.trim();
        (!boot.is_empty()).then(|| boot.to_owned())
    }

    /// At start: a host that booted since runners skipped their syncs, with no clean stop between, may have lost what
    /// they wrote, unless that boot ended in an orderly shutdown, whose final sync wrote everything once every runner
    /// had stopped. A boot ID that cannot be read now cannot be told from another boot.
    pub(crate) fn recovered(&mut self) -> io::Result<Recovered> {
        let Owed::Since { boot } = &self.state else {
            return Ok(Recovered::Unchanged);
        };
        if self.boot().as_deref() == Some(boot.as_str()) {
            return Ok(Recovered::Unchanged);
        }
        if (self.durability.0.ended_synced)(boot) {
            self.settle()?;
            return Ok(Recovered::ShutDownInOrder);
        }
        self.state = Owed::Torn {
            refusals: 0,
            held: Vec::new(),
        };
        write(&self.file, TORN)?;
        Ok(Recovered::Torn)
    }

    /// Whether the caches must be emptied before any runner starts.
    pub(crate) fn is_torn(&self) -> bool {
        matches!(self.state, Owed::Torn { .. })
    }

    /// While the caches are torn, the volumes a container kept the last try from emptying: none before a try, or
    /// after one Docker failed.
    pub(crate) fn torn(&self) -> Option<&[String]> {
        match &self.state {
            Owed::Torn { held, .. } => Some(held),
            Owed::Synced | Owed::Since { .. } => None,
        }
    }

    /// Before a runner that may skip its syncs starts: whether it may, once that is kept on disk. A runner that may not
    /// keeps its syncs: while the caches are torn none starts, and where the boot ID or the file cannot be had, a
    /// later crash could not be told.
    pub(crate) fn runner_skips(&mut self) -> bool {
        let Some(boot) = self.boot() else {
            return false;
        };
        match &self.state {
            Owed::Torn { .. } => false,
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
        if !self.is_torn() {
            return Ok(());
        }
        self.settle()
    }

    /// A container left on each of `volumes`, torn caches, kept them from being emptied. After `REFUSALS` of them the engine may give up
    /// and start runners on what the caches hold, as a cache that may be torn costs less than no runner at all; it
    /// stays torn until it does (`gave_up`).
    pub(crate) fn refused(&mut self, volumes: Vec<String>) -> AfterRefusal {
        let Owed::Torn { refusals, held } = &mut self.state else {
            return AfterRefusal::GiveUp;
        };
        *refusals = refusals.saturating_add(1);
        *held = volumes;
        let tries = *refusals;
        if tries < REFUSALS {
            return AfterRefusal::Again { tries };
        }
        AfterRefusal::GiveUp
    }

    /// The engine gave up on the caches it could not empty, once no slot's layers among them are left as they are:
    /// once that is on disk, nothing is owed.
    pub(crate) fn gave_up(&mut self) -> io::Result<()> {
        if !self.is_torn() {
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
        (self.durability.0.flush)();
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
