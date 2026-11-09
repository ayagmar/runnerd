//! Whether the registry mirror runs, as far as the engine knows, and the memory cap it counts at. It owns that one
//! value and never calls Docker: the engine does, and tells it what Docker answered.

use crate::cache;

/// The registry mirror as the engine knows it. Nothing survives a restart: a start begins not knowing, as an earlier
/// run may have left the mirror up, and recovery asks Docker. A reload keeps it, as a running mirror keeps the cap it
/// started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mirror(Phase);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Recovery has not asked Docker, or could not read its answer: an earlier run may have left it up. `cap` is the
    /// configured cap, the most it can be sure of.
    Unknown { cap: u64 },
    /// Docker says none runs, but a stopped container may be left.
    Absent,
    /// It runs, with this memory cap.
    Up { cap: u64 },
    /// It is stopped, and no container of it is left.
    Down,
}

impl Mirror {
    /// At the daemon's start, with an earlier run's mirror counted at the configured cap, the most it can be sure of.
    pub(super) fn unknown(configured: u64) -> Mirror {
        Mirror(Phase::Unknown { cap: configured })
    }

    /// Docker says it runs, with `memory` if it says what cap.
    pub(super) fn found_running(&mut self, memory: Option<u64>) {
        let cap = memory.unwrap_or(self.running_memory());
        self.0 = Phase::Up { cap };
    }

    /// Docker says no mirror runs.
    pub(super) fn found_not_running(&mut self) {
        self.0 = Phase::Absent;
    }

    /// The engine started it, or found it up, with this cap.
    pub(super) fn started(&mut self, cap: u64) {
        self.0 = Phase::Up { cap };
    }

    /// The engine stopped it.
    pub(super) fn stopped(&mut self) {
        self.0 = Phase::Down;
    }

    /// Whether a runner about to start must start it first.
    pub(super) fn needs_start(self) -> bool {
        self.0 == Phase::Down
    }

    /// Whether it may run: there may be something to stop or to read.
    pub(super) fn may_run(self) -> bool {
        self.0 != Phase::Down
    }

    /// What a check of the shared caches may do with its storage.
    pub(super) fn storage(self) -> cache::Mirror {
        if self.may_run() {
            cache::Mirror::Up
        } else {
            cache::Mirror::Down
        }
    }

    /// The memory cap a running mirror holds; none when it is down.
    pub(super) fn running_memory(self) -> u64 {
        match self.0 {
            Phase::Unknown { cap } | Phase::Up { cap } => cap,
            Phase::Absent | Phase::Down => 0,
        }
    }
}
