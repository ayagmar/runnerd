//! What the engine's tests read and write of its private state, in one place: a change to the shape of a field changes
//! the body here, and no test. Nothing here decides anything; each only reads or sets what a test names.

use std::time::{Duration, SystemTime};

use super::*;
use crate::scheduler::engine::Occupant;
use crate::scheduler::engine::admission::Admitting;
use crate::scheduler::{Hold, Key, Wait, Waits};

/// What a pool's hold holds.
#[derive(Debug, Clone, Copy)]
pub(super) struct HoldFacts {
    pub(super) held: Option<Hold>,
    pub(super) failures: u32,
}

impl<G: GithubApi, D: DockerApi> Engine<G, D> {
    pub(super) fn occupants_mut(&mut self) -> impl Iterator<Item = &mut Occupant> {
        self.slots.values_mut().flatten().flatten()
    }

    pub(super) fn pool_hold(&self, repository: &str, profile: &str) -> Option<HoldFacts> {
        let hold = self.repositories.get(repository)?.holds.get(profile)?;
        Some(HoldFacts {
            held: hold.held,
            failures: hold.failures,
        })
    }

    /// Whether a pool holds nothing back and no streak of runners that left is under way.
    pub(super) fn pool_is_quiet(&self, repository: &str, profile: &str, now: SystemTime) -> bool {
        self.repositories
            .get(repository)
            .and_then(|state| state.holds.get(profile))
            .is_none_or(|hold| {
                hold.idle_exits == 0 && hold.held.is_none_or(|held| held.until <= now)
            })
    }

    pub(super) fn set_held(&mut self, repository: &str, profile: &str, held: Hold) {
        let state = self.repositories.get_mut(repository).unwrap();
        state.holds.entry(profile.to_owned()).or_default().held = Some(held);
    }

    pub(super) fn clear_holds(&mut self) {
        for state in self.repositories.values_mut() {
            for hold in state.holds.values_mut() {
                hold.held = None;
            }
        }
    }

    pub(super) fn expire_holds(&mut self, past: SystemTime) {
        for state in self.repositories.values_mut() {
            for hold in state.holds.values_mut() {
                if let Some(held) = hold.held.as_mut() {
                    held.until = past;
                }
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
        state.waits.entry(profile.to_owned()).or_default().memory = Some(wait);
    }

    /// As if every wait began `by` earlier.
    pub(super) fn age_waits(&mut self, by: Duration) {
        for state in self.repositories.values_mut() {
            for waits in state.waits.values_mut() {
                if let Some(wait) = waits.memory.as_mut() {
                    wait.since -= by;
                }
                if let Some(wait) = waits.cpu.as_mut() {
                    wait.since -= by;
                }
            }
        }
    }

    pub(super) fn reseeding(&self, profile: &str) -> bool {
        self.reseeds.contains_key(profile)
    }

    pub(super) fn no_reseeds(&self) -> bool {
        self.reseeds.is_empty()
    }

    /// The memory of each seed container that runs, by profile.
    pub(super) fn seed_memories(&self) -> BTreeMap<String, u64> {
        self.reseed_memory.clone()
    }

    pub(super) fn reseed_is_requested(&self, profile: &str) -> bool {
        self.reseed_requested.contains(profile)
    }

    pub(super) fn ask_reseed(&mut self, profile: &str) {
        self.reseed_requested.insert(profile.to_owned());
    }

    pub(super) fn make_reseed_due(&mut self, profile: &str) {
        self.reseed_due.insert(profile.to_owned());
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
        self.measured.host.short
    }

    pub(super) fn set_host_short(&mut self) {
        self.measured.host.short = true;
    }

    pub(super) fn waits_for_a_slot(&self, job: u64) -> bool {
        self.measured.slot_waiting.0.contains(&job)
    }

    pub(super) fn waits_for_the_host(&self, job: u64) -> bool {
        self.measured.host_waiting.0.contains(&job)
    }

    pub(super) fn wait_for_the_host(&mut self, job: u64) {
        self.measured.host_waiting.0.insert(job);
    }

    pub(super) fn raise(&mut self, runner: String) {
        self.measured.raised.push(runner);
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
            oversized: self.oversized.len(),
            unread: self.unread.len(),
            split_waits: self
                .repositories
                .values()
                .flat_map(|state| state.waits.values())
                .filter(|waits| {
                    waits
                        .memory
                        .zip(waits.cpu)
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
    plan.remembered.short = short;
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
    pub(super) fn listed_busy(&self) -> bool {
        self.busy
    }

    pub(super) fn named_job(&self) -> Option<u64> {
        self.job
    }

    /// When its capture found its job; `None` before.
    pub(super) fn job_found_at(&self) -> Option<SystemTime> {
        self.job_started
    }

    pub(super) fn age_job_start(&mut self, by: Duration) {
        self.job_started = self.job_started.map(|at| at - by);
    }

    pub(super) fn has_taken(&mut self, job: u64) {
        self.job = Some(job);
        self.busy = true;
    }

    pub(super) fn set_key(&mut self, key: Key) {
        self.key = Some(key);
    }

    pub(super) fn has_key(&self) -> bool {
        self.key.is_some()
    }

    pub(super) fn was_started_here(&self) -> bool {
        self.purpose.is_some()
    }

    pub(super) fn is_observed(&self) -> bool {
        self.observed
    }
}
