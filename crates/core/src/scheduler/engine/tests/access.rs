//! What the engine's tests read and write of its private state, in one place: a change to the shape of a field changes
//! the body here, and no test. Nothing here decides anything; each only reads or sets what a test names.

use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::*;
use crate::admission::{Predictor, host};
use crate::scheduler::engine::Occupant;
use crate::scheduler::engine::admission::Admitting;
use crate::scheduler::engine::history::{Capture, Found};
use crate::scheduler::engine::open_waits::WaitFor;
use crate::scheduler::engine::upkeep::Due;
use crate::scheduler::{Hold, Key, Wait, Waits};
use crate::store::LogPosition;

/// What a pool's hold holds.
#[derive(Debug, Clone, Copy)]
pub(super) struct HoldFacts {
    pub(super) held: Option<Hold>,
    pub(super) failures: u32,
    pub(super) idle_exits: u32,
}

impl<G: GithubApi, D: DockerApi> Engine<G, D> {
    pub(super) fn occupants_mut(&mut self) -> impl Iterator<Item = &mut Occupant> {
        self.slots.values_mut().flatten().flatten()
    }

    pub(super) fn pool_hold(&self, repository: &str, profile: &str) -> Option<HoldFacts> {
        let hold = self.repositories.get(repository)?.holds.get(profile)?;
        Some(HoldFacts {
            held: hold.held(),
            failures: hold.failures(),
            idle_exits: hold.idle_exits(),
        })
    }

    /// Whether a pool holds nothing back and no streak of runners that left is under way.
    pub(super) fn pool_is_quiet(&self, repository: &str, profile: &str, now: SystemTime) -> bool {
        self.repositories
            .get(repository)
            .and_then(|state| state.holds.get(profile))
            .is_none_or(|hold| hold.idle_exits() == 0 && !hold.holding(now))
    }

    /// Sets a pool's two streaks: runners that failed, and runners that left without a job.
    pub(super) fn set_streaks(
        &mut self,
        repository: &str,
        profile: &str,
        failures: u32,
        idle_exits: u32,
    ) {
        let state = self.repositories.get_mut(repository).unwrap();
        state
            .holds
            .entry(profile.to_owned())
            .or_default()
            .set_streaks(failures, idle_exits);
    }

    /// Makes `capture` the log capture of `runner`, whatever one it had.
    pub(super) fn set_capture(&mut self, runner: &str, capture: JoinHandle<Capture>) {
        self.captures.insert(runner.to_owned(), capture);
    }

    /// Where a runner's capture reports the job it found.
    pub(super) fn found_sender(&self) -> mpsc::UnboundedSender<Found> {
        self.found.0.clone()
    }

    /// How many runners gone count against a job listed for `repository` without the engine knowing which.
    pub(super) fn unnamed_claims(&self, repository: &str) -> usize {
        self.claims
            .get(repository)
            .map_or(0, |claims| claims.unnamed.len())
    }

    pub(super) fn set_held(&mut self, repository: &str, profile: &str, held: Hold) {
        let state = self.repositories.get_mut(repository).unwrap();
        state
            .holds
            .entry(profile.to_owned())
            .or_default()
            .set_held(Some(held));
    }

    pub(super) fn clear_holds(&mut self) {
        for state in self.repositories.values_mut() {
            for hold in state.holds.values_mut() {
                hold.set_held(None);
            }
        }
    }

    pub(super) fn expire_holds(&mut self, past: SystemTime) {
        for state in self.repositories.values_mut() {
            for hold in state.holds.values_mut() {
                hold.expire(past);
            }
        }
    }

    pub(super) fn set_queued(&mut self, repository: &str, jobs: Vec<QueuedJob>) {
        self.repositories.get_mut(repository).unwrap().queued = jobs;
    }

    pub(super) fn set_listed(&mut self, repository: &str, jobs: Vec<QueuedJob>) {
        let state = self.repositories.get_mut(repository).unwrap();
        state.queued = jobs;
        state.listed = true;
    }

    /// The jobs each repository whose last listing succeeded has queued, with what its pools keep.
    pub(super) fn listed_repositories(&self) -> Vec<ListedFacts<'_>> {
        self.repositories
            .iter()
            .filter(|(_, state)| state.listed)
            .map(|(name, state)| ListedFacts {
                name,
                queued: &state.queued,
                waits: state.waits.values().copied().collect(),
            })
            .collect()
    }

    pub(super) fn waits_of(&self, repository: &str, profile: &str) -> Option<Waits> {
        self.repositories[repository].waits.get(profile).copied()
    }

    pub(super) fn keeps_no_waits(&self, repository: &str) -> bool {
        self.repositories[repository].waits.is_empty()
    }

    pub(super) fn set_memory_wait(&mut self, repository: &str, profile: &str, wait: Wait) {
        let state = self.repositories.get_mut(repository).unwrap();
        let waits = state.waits.entry(profile.to_owned()).or_default();
        *waits = waits.with_memory(wait);
    }

    pub(super) fn set_cpu_wait(&mut self, repository: &str, profile: &str, wait: Wait) {
        let state = self.repositories.get_mut(repository).unwrap();
        let waits = state.waits.entry(profile.to_owned()).or_default();
        *waits = waits.with_cpu(wait);
    }

    /// As if every wait began `by` earlier.
    pub(super) fn age_waits(&mut self, by: Duration) {
        for state in self.repositories.values_mut() {
            for waits in state.waits.values_mut() {
                if let Some(wait) = waits.memory_mut() {
                    wait.since -= by;
                }
                if let Some(wait) = waits.cpu_mut() {
                    wait.since -= by;
                }
            }
        }
    }

    pub(super) fn reseeding(&self, profile: &str) -> bool {
        self.upkeep.reseed_runs(profile)
    }

    pub(super) fn no_reseeds(&self) -> bool {
        self.upkeep.seed_memories().is_empty()
    }

    /// The memory of each seed container that runs, by profile.
    pub(super) fn seed_memories(&self) -> BTreeMap<String, u64> {
        self.upkeep.seed_memories()
    }

    /// A seed container of `profile` runs with this cap, and goes on running.
    pub(super) fn seed_runs(&mut self, profile: &str, memory: u64) {
        self.upkeep
            .reseed_started(profile, tokio::spawn(std::future::pending()), memory);
    }

    pub(super) fn is_retired(&self, profile: &str) -> bool {
        self.retired.contains_key(profile)
    }

    pub(super) fn reseed_is_requested(&self, profile: &str) -> bool {
        self.upkeep.is_requested(profile)
    }

    pub(super) fn ask_reseed(&mut self, profile: &str) {
        self.upkeep.request(profile);
    }

    pub(super) fn make_reseed_due(&mut self, profile: &str) {
        self.upkeep.mark(profile, Due::Changed);
    }

    pub(super) fn is_leftover(&self, container: &str) -> bool {
        self.leftover.contains_key(container)
    }

    pub(super) fn no_leftovers(&self) -> bool {
        self.leftover.is_empty()
    }

    /// Ends every log capture, as a daemon's exit does.
    pub(super) fn abort_captures(&mut self) {
        for (_, capture) in std::mem::take(&mut self.captures) {
            capture.abort();
        }
    }

    /// As if every runner holding uploads ended `by` earlier.
    pub(super) fn age_ended_uploads(&mut self, by: Duration) {
        for ended in self.ended_uploads.values_mut() {
            ended.ended_at -= by;
        }
    }

    pub(super) fn no_ended_uploads(&self) -> bool {
        self.ended_uploads.is_empty()
    }

    pub(super) fn host_is_short(&self) -> bool {
        self.measured.host.is_short()
    }

    pub(super) fn set_host_short(&mut self) {
        if self.measured.host == host::HostBand::Plain {
            self.measured.host = host::HostBand::Larger;
        }
    }

    pub(super) fn waits_for_a_slot(&self, job: u64) -> bool {
        self.measured.job_waits.jobs(WaitFor::Slot).contains(&job)
    }

    pub(super) fn waits_for_the_host(&self, job: u64) -> bool {
        self.measured.job_waits.jobs(WaitFor::Host).contains(&job)
    }

    pub(super) fn wait_for_the_host(&mut self, job: u64) {
        let mut waiting = self.measured.job_waits.jobs(WaitFor::Host);
        waiting.insert(job);
        self.measured
            .job_waits
            .open_wait(WaitFor::Host, waiting, crate::time::now_ms());
    }

    pub(super) fn raise(&mut self, runner: String) {
        self.measured.raised.push(runner);
    }

    pub(super) fn raised(&self) -> &[String] {
        &self.measured.raised
    }

    pub(super) fn measure_is_due(&self, profile: &str) -> bool {
        self.upkeep.measure_is_due(profile)
    }

    pub(super) fn forget_predictor(&mut self) {
        self.measured.predictor = None;
    }

    pub(super) fn set_predictor(&mut self, predictor: Predictor, read_at: SystemTime) {
        self.measured.predictor = Some((predictor, read_at));
    }

    pub(super) fn no_recordings(&self) -> bool {
        !self.records.is_recording()
    }

    pub(super) fn no_shells(&self) -> bool {
        !self.records.has_shells()
    }

    pub(super) fn is_closing(&self, job: u64) -> bool {
        self.closing.contains_key(&job)
    }

    /// As if the runner of `job` was found gone at `ms` since the epoch.
    pub(super) fn set_closing_since(&mut self, job: u64, ms: u64) {
        if let Some(closing) = self.closing.get_mut(&job) {
            closing.found_gone_at(ms);
        }
    }

    /// As if every broken capture last stored up to `position`, ahead of or behind what the store has.
    pub(super) fn set_recapture_resumes(&mut self, position: LogPosition) {
        for recapture in self.recaptures.values_mut() {
            recapture.resume_at(position);
        }
    }

    /// The runners' names this machine registers begin with this.
    pub(super) fn runner_prefix(&self) -> &str {
        &self.runner_prefix
    }

    pub(super) fn make_trim_due(&mut self) {
        self.trim_due = true;
    }

    /// As if every forced pause's grace ran out.
    pub(super) fn expire_cancelling(&mut self) {
        let now = tokio::time::Instant::now();
        for deadline in self.cancelling.values_mut() {
            *deadline = now;
        }
    }

    /// As if every unread leftover's backoff ran out.
    pub(super) fn make_unread_due(&mut self) {
        let now = tokio::time::Instant::now();
        for left in &mut self.unread {
            left.due = now;
        }
    }

    pub(super) fn state_counts(&self) -> StateCounts {
        StateCounts {
            closing: self.closing.len(),
            shared_with_queued: self
                .closing
                .keys()
                .filter(|id| self.queued_jobs.contains_key(*id))
                .count(),
            ended_uploads: self.ended_uploads.len(),
            cancelling: self.cancelling.len(),
            busy_orphans: self.busy_orphans.len(),
            oversized: self.upkeep.oversized(),
            unread: self.unread.len(),
            split_waits: self
                .repositories
                .values()
                .flat_map(|state| state.waits.values())
                .filter(|waits| {
                    waits
                        .memory()
                        .zip(waits.cpu())
                        .is_some_and(|(a, b)| a.job != b.job)
                })
                .count(),
            captures_with_recaptures: self
                .captures
                .keys()
                .filter(|runner| self.recaptures.contains_key(*runner))
                .count(),
        }
    }
}

/// Sets what a plan remembers of the host's margin.
pub(super) fn set_plan_short(plan: &mut Admitting, short: bool) {
    plan.remembered = host::Remembered::testing(short, plan.remembered.was_stalled());
}

/// How much of each state the invariants' steps exist to reach the engine holds right now.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StateCounts {
    pub(super) closing: usize,
    pub(super) shared_with_queued: usize,
    pub(super) ended_uploads: usize,
    pub(super) cancelling: usize,
    pub(super) busy_orphans: usize,
    pub(super) oversized: usize,
    pub(super) unread: usize,
    pub(super) split_waits: usize,
    pub(super) captures_with_recaptures: usize,
}

/// A repository whose last listing succeeded.
pub(super) struct ListedFacts<'engine> {
    pub(super) name: &'engine str,
    pub(super) queued: &'engine [QueuedJob],
    pub(super) waits: Vec<Waits>,
}

impl Occupant {
    /// When its capture found its job; `None` before.
    pub(super) fn job_found_at(&self) -> Option<SystemTime> {
        self.job_started
    }

    pub(super) fn found_job_at(&mut self, at: SystemTime) {
        self.job_started = Some(at);
    }

    pub(super) fn age_job_start(&mut self, by: Duration) {
        self.job_started = self.job_started.map(|at| at - by);
    }

    /// As a listing shows it busy, with no job named.
    pub(super) fn set_listed_busy(&mut self) {
        self.busy = true;
    }

    pub(super) fn has_taken(&mut self, job: u64) {
        self.job = Some(job);
        self.busy = true;
    }

    pub(super) fn set_key(&mut self, key: Key) {
        self.key = Some(key);
    }
}
