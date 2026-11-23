//! What the engine does when the host wakes from a sleep (ADR 0159), and what it holds back until it has settled. A
//! sleep stops the monotonic clock tokio's timers and the governor go by, while the wall clock the waits, the store and
//! the idle runners go by jumps on: left alone, hours of sleep read as hours of waiting, pre-sleep readings as stale
//! samples, and runners whose sessions GitHub ended as runners ready for a job. `sleep::Sleep` owns the state; this
//! adds the engine's side: it reads the clocks, tells the store and undoes what the sleep made wrong.

use tracing::{info, warn};

use super::Engine;
use crate::api::duration;
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::scheduler::{Decision, StopReason};
use crate::time;

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Looks whether the host slept since the last look, by how far the time it spent suspended rose; a step of the
    /// wall clock moves nothing it reads. It looks wherever the engine resumes: first in a tick and in a pass, again
    /// after a tick's listings and before what the tick records, when a runner's capture reports a take, and before a
    /// command, a start's launch or a failure that counts against a pool. A look is two clock reads.
    ///
    /// After a sleep: the store takes it as downtime, the open waits end where it began, the governor's readings and
    /// the growth count's looks go, every pool's waits end, and every repository's queued jobs are unknown until it is
    /// listed again, which it is at once, with its runners. Starts wait until the engine has settled
    /// (`holds_starts`), and the runners that were up stop once idle (`slept_stops`).
    pub(super) fn look_for_sleep(&mut self) {
        let present: Vec<String> = self
            .occupied()
            .map(|(_, occupant)| occupant.runner_name.clone())
            .collect();
        let sampled = self.store.latest_host_sample().is_some();
        let Some(slept) = self.sleep.look(
            self.clock.suspended_ms(),
            self.clock.now_ms(),
            present,
            sampled,
        ) else {
            return;
        };
        info!(
            "the host slept for {}: that time is downtime, not waiting; no runner starts until the host is sampled and \
             GitHub listed again, and the runners that were up stop once idle",
            duration(slept / 1000)
        );
        // What is stored goes by the system's clock, which moved on by the sleep.
        let until = time::now_ms();
        let from = until.saturating_sub(slept);
        self.store.downtime(from, until);
        self.note_slept(from);
        self.governor.slept();
        for (_, state) in self.repositories.iter_listed_mut() {
            state.forget_waits(|_| false);
            state.listing_mut().slept();
            state.pace_mut().slept();
        }
    }

    /// After a restart: the adopted runners that started before the host's last sleep began, which the history keeps,
    /// lived through it, and GitHub may have ended their sessions. A restart between a wake and their stop must not
    /// count them as covering a queued job again.
    pub(super) async fn recover_sleep(&mut self) {
        let began = match self.store.last_sleep_began().await {
            Ok(Some(began)) => began,
            Ok(None) => return,
            Err(error) => {
                warn!("cannot read when the host last slept: {error}");
                return;
            }
        };
        let predating: Vec<String> = self
            .occupied()
            .filter(|(_, occupant)| time::ms(occupant.started_at) < began)
            .map(|(_, occupant)| occupant.runner_name.clone())
            .collect();
        if !predating.is_empty() {
            info!(
                "{} adopted runners started before the host last slept: they stop once idle",
                predating.len()
            );
        }
        self.sleep.predate(predating);
    }

    /// Whether the last wake holds every start back, and keeps a failed start or a runner that leaves without a job
    /// from counting against its pool: until a host sample from after it is in and a listing of queued jobs answered.
    pub(super) fn holds_starts(&self) -> bool {
        self.sleep
            .settling(self.store.latest_host_sample().map(|sample| sample.at))
    }

    /// Whether `runner` was up when the host last slept.
    pub(super) fn slept_through(&self, runner: &str) -> bool {
        self.sleep.predates(runner)
    }

    /// A listing of queued jobs that began at `began` (`Clock::now_ms`) answered: the wake waits for it no more, if
    /// it began after the wake.
    pub(super) fn listed_after_wake(&mut self, began: u64) {
        self.sleep.listed(began);
    }

    /// Ends the wake's hold once nothing holds it.
    pub(super) fn settle_wake(&mut self) {
        if self
            .sleep
            .settle(self.store.latest_host_sample().map(|sample| sample.at))
        {
            info!("settled after the host woke: runners start as the budget allows");
        }
    }

    /// The idle runners that were up before the host last slept, to stop, except those `decided` stops already.
    pub(super) fn slept_stops(&self, decided: &[Decision]) -> Vec<Decision> {
        let stopped = |slot: &crate::scheduler::SlotRef| {
            decided
                .iter()
                .any(|decision| matches!(decision, Decision::Stop { slot: stopping, .. } if stopping == slot))
        };
        self.occupied()
            .filter(|(slot, occupant)| {
                occupant.idle() && self.sleep.predates(&occupant.runner_name) && !stopped(slot)
            })
            .map(|(slot, _)| Decision::Stop {
                slot,
                reason: StopReason::Slept,
            })
            .collect()
    }
}
