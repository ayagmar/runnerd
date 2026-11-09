//! How far the daemon's stop has got: how it was asked to stop, and whether Docker stopped answering it. It owns that
//! one value, which `Control` publishes, and decides what the engine does with it; it never calls Docker.

use super::control::Shutdown;

/// The daemon's stop. Nothing survives a restart, as a new daemon keeps scheduling and adopts or removes what a stop
/// left, and a reload does not change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stop(Phase);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Docker answers, if with errors, or no stop was asked for: `Shutdown::No` until one is.
    Asked(Shutdown),
    /// Docker did not answer a call the stop needed. The stop calls it no more and goes on as a detach, whatever was
    /// asked for or is asked for later.
    DockerGone,
}

impl Stop {
    /// A daemon that keeps scheduling.
    pub(super) fn none() -> Stop {
        Stop(Phase::Asked(Shutdown::No))
    }

    /// A request to stop in `mode`: a more urgent one overrides a less urgent one, never the other way. Returns whether
    /// that changed anything.
    pub(super) fn ask(&mut self, mode: Shutdown) -> bool {
        match &mut self.0 {
            Phase::Asked(asked) if mode > *asked => {
                *asked = mode;
                true
            }
            Phase::Asked(_) | Phase::DockerGone => false,
        }
    }

    /// Docker did not answer a call the stop needed. Returns whether that is news: not without a stop under way, nor
    /// once noted.
    pub(super) fn note_docker_gone(&mut self) -> bool {
        match self.0 {
            Phase::Asked(asked) if asked != Shutdown::No => {
                self.0 = Phase::DockerGone;
                true
            }
            Phase::Asked(_) | Phase::DockerGone => false,
        }
    }

    /// How the stop goes on: as requested, or as a detach once Docker stopped answering it.
    pub(super) fn mode(self) -> Shutdown {
        match self.0 {
            Phase::Asked(asked) => asked,
            Phase::DockerGone => Shutdown::Detach,
        }
    }

    /// Whether Docker stopped answering the stop: it is left to the next start, which removes or adopts every
    /// container by its labels.
    pub(super) fn docker_gone(self) -> bool {
        self.0 == Phase::DockerGone
    }

    /// Whether the stop has a deadline: it detaches or is forced.
    pub(super) fn hurried(self) -> bool {
        self.mode() >= Shutdown::Detach
    }
}
