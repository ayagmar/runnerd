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
use crate::scheduler::engine::slot::Slot;
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
        self.slots
            .values_mut()
            .flatten()
            .filter_map(Slot::occupant_mut)
    }

    pub(super) fn pool_hold(&self, repository: &str, profile: &str) -> Option<HoldFacts> {
        let hold = self.repositories.listed(repository)?.health(profile)?;
        Some(HoldFacts {
            held: hold.held(),
            failures: hold.failures(),
            idle_exits: hold.idle_exits(),
        })
    }

    /// Whether a pool holds nothing back and no streak of runners that left is under way.
    pub(super) fn pool_is_quiet(&self, repository: &str, profile: &str, now: SystemTime) -> bool {
        self.repositories
            .listed(repository)
            .and_then(|state| state.health(profile))
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
        let state = self.repositories.listed_mut(repository).unwrap();
        state.health_mut(profile).set_streaks(failures, idle_exits);
    }

    /// Makes `capture` the log capture of `runner`, whatever one it had.
    pub(super) fn set_capture(&mut self, runner: &str, capture: JoinHandle<Capture>) {
        self.watches.replace_capture(runner, capture);
    }

    /// Where a runner's capture reports the job it found.
    pub(super) fn found_sender(&self) -> mpsc::UnboundedSender<Found> {
        self.found.0.clone()
    }

    /// Whether any claim on `repository`'s listed jobs is remembered.
    pub(super) fn has_claims(&self, repository: &str) -> bool {
        self.claims.has(repository)
    }

    /// Whether this engine remembers having ended `runner`.
    pub(super) fn ended_here(&self, runner: u64) -> bool {
        self.registrations.is_ended_here(runner)
    }

    /// How many runners gone count against a job listed for `repository` without the engine knowing which.
    pub(super) fn unnamed_claims(&self, repository: &str) -> usize {
        self.claims.unnamed_of(repository)
    }

    pub(super) fn set_held(&mut self, repository: &str, profile: &str, held: Hold) {
        let state = self.repositories.listed_mut(repository).unwrap();
        state.health_mut(profile).set_held(held);
    }

    pub(super) fn clear_holds(&mut self) {
        for (_, state) in self.repositories.iter_listed_mut() {
            for hold in state.all_health_mut() {
                hold.expire(SystemTime::UNIX_EPOCH);
            }
        }
    }

    pub(super) fn expire_holds(&mut self, past: SystemTime) {
        for (_, state) in self.repositories.iter_listed_mut() {
            for hold in state.all_health_mut() {
                hold.expire(past);
            }
        }
    }

    /// Moves the end of a used-up GitHub rate limit, if one is noted, to `past`.
    pub(super) fn expire_github_limit(&mut self, past: SystemTime) {
        if let Some(until) = self.github_limited_until.as_mut() {
            *until = past;
        }
    }

    pub(super) fn set_queued(&mut self, repository: &str, jobs: Vec<QueuedJob>) {
        self.repositories
            .listed_mut(repository)
            .unwrap()
            .listing_mut()
            .replace_jobs(jobs);
    }

    pub(super) fn set_listed(&mut self, repository: &str, jobs: Vec<QueuedJob>) {
        let state = self.repositories.listed_mut(repository).unwrap();
        state.listing_mut().answer(jobs);
    }

    /// The jobs each repository whose last listing succeeded has queued, with what its pools keep.
    pub(super) fn listed_repositories(&self) -> Vec<ListedFacts<'_>> {
        self.repositories
            .iter_listed()
            .filter_map(|(name, state)| {
                Some(ListedFacts {
                    name,
                    queued: state.listing().listed()?,
                    waits: state.all_waits().copied().collect(),
                })
            })
            .collect()
    }

    pub(super) fn waits_of(&self, repository: &str, profile: &str) -> Option<Waits> {
        self.repositories
            .listed(repository)?
            .waits(profile)
            .copied()
    }

    pub(super) fn keeps_no_waits(&self, repository: &str) -> bool {
        self.repositories
            .listed(repository)
            .unwrap()
            .all_waits()
            .next()
            .is_none()
    }

    pub(super) fn set_memory_wait(&mut self, repository: &str, profile: &str, wait: Wait) {
        let state = self.repositories.listed_mut(repository).unwrap();
        let waits = state.waits_entry(profile);
        *waits = waits.with_memory(wait);
    }

    pub(super) fn set_cpu_wait(&mut self, repository: &str, profile: &str, wait: Wait) {
        let state = self.repositories.listed_mut(repository).unwrap();
        let waits = state.waits_entry(profile);
        *waits = waits.with_cpu(wait);
    }

    /// As if every wait began `by` earlier.
    pub(super) fn age_waits(&mut self, by: Duration) {
        for (_, state) in self.repositories.iter_listed_mut() {
            for waits in state.all_waits_mut() {
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
        self.leftovers.is_removing(container)
    }

    pub(super) fn no_leftovers(&self) -> bool {
        self.leftovers.to_remove().is_empty()
    }

    /// Ends every log capture, as a daemon's exit does.
    pub(super) fn abort_captures(&mut self) {
        self.watches.abort_all();
    }

    /// As if the running jobs were last read `by` earlier, so the next tick reads them.
    pub(super) fn age_running_reads(&mut self, by: Duration) {
        self.running_reads.age(by);
    }

    /// As if every runner holding uploads ended `by` earlier.
    pub(super) fn age_ended_uploads(&mut self, by: Duration) {
        for ended in self.ended_uploads.values_mut() {
            ended.ended_at -= by;
        }
    }

    /// The runners whose build cache uploads wait for GitHub's word, short of `ENDED_WAIT`.
    pub(super) fn ended_uploads_waiting(&self) -> BTreeSet<String> {
        self.ended_uploads
            .iter()
            .filter(|(_, ended)| {
                ended
                    .ended_at
                    .elapsed()
                    .is_ok_and(|waited| waited < crate::scheduler::engine::build_cache::ENDED_WAIT)
            })
            .map(|(runner, _)| runner.clone())
            .collect()
    }

    /// The runners whose build cache uploads wait.
    pub(super) fn ended_upload_runners(&self) -> BTreeSet<String> {
        self.ended_uploads.keys().cloned().collect()
    }

    pub(super) fn no_ended_uploads(&self) -> bool {
        self.ended_uploads.is_empty()
    }

    pub(super) fn host_is_short(&self) -> bool {
        self.measured.host().is_short()
    }

    pub(super) fn set_host_short(&mut self) {
        self.measured.set_host_short();
    }

    pub(super) fn waits_for_a_slot(&self, job: u64) -> bool {
        self.measured.waiting(WaitFor::Slot).contains(&job)
    }

    pub(super) fn waits_for_the_host(&self, job: u64) -> bool {
        self.measured.waiting(WaitFor::Host).contains(&job)
    }

    pub(super) fn wait_for_the_host(&mut self, job: u64) {
        self.measured.wait_for_the_host(job);
    }

    pub(super) fn raise(&mut self, runner: String) {
        self.measured.raise(runner);
    }

    pub(super) fn raised(&self) -> &[String] {
        self.measured.raised()
    }

    pub(super) fn measure_is_due(&self, profile: &str) -> bool {
        self.upkeep.measure_is_due(profile)
    }

    pub(super) fn forget_predictor(&mut self) {
        self.measured.set_predictor(None);
    }

    pub(super) fn set_predictor(&mut self, predictor: Predictor, read_at: SystemTime) {
        self.measured.set_predictor(Some((predictor, read_at)));
    }

    pub(super) fn no_recordings(&self) -> bool {
        !self.records.is_recording()
    }

    pub(super) fn no_shells(&self) -> bool {
        !self.records.has_shells()
    }

    pub(super) fn is_closing(&self, job: u64) -> bool {
        self.ledger.is_closing(job)
    }

    /// Whether `job` is followed as a queued job.
    pub(super) fn is_followed_as_queued(&self, job: u64) -> bool {
        self.ledger.is_queued(job)
    }

    /// As if `by` passed on the clocks of the jobs followed: since a closing job's runner was found gone, and since
    /// GitHub began failing the reads of a queued job that left the queue.
    pub(super) fn age_followed_jobs(&mut self, by: Duration) {
        self.ledger.age(by);
    }

    /// As if the runner of `job` was found gone at `ms` since the epoch.
    pub(super) fn set_closing_since(&mut self, job: u64, ms: u64) {
        self.ledger.found_gone_at(job, ms);
    }

    /// As if every broken capture last stored up to `position`, ahead of or behind what the store has.
    pub(super) fn set_recapture_resumes(&mut self, position: LogPosition) {
        self.watches.resume_every_break_at(position);
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
        for occupant in self
            .slots
            .values_mut()
            .flatten()
            .filter_map(Slot::occupant_mut)
        {
            occupant.engagement.expire_cancel(now);
        }
    }

    /// As if every unread leftover's backoff ran out.
    pub(super) fn make_unread_due(&mut self) {
        self.leftovers.make_due();
    }

    pub(super) fn state_counts(&self) -> StateCounts {
        StateCounts {
            closing: self.ledger.closing().len(),
            ended_uploads: self.ended_uploads.len(),
            cancelling: self
                .slots
                .values()
                .flatten()
                .filter_map(Slot::occupant)
                .filter(|occupant| occupant.engagement.is_cancelling())
                .count(),
            busy_orphans: self.registrations.orphans(),
            oversized: self.upkeep.oversized(),
            unread: self.leftovers.unread(),
            split_waits: self
                .repositories
                .iter_listed()
                .flat_map(|(_, state)| state.all_waits())
                .filter(|waits| {
                    waits
                        .memory()
                        .zip(waits.cpu())
                        .is_some_and(|(a, b)| a.job != b.job)
                })
                .count(),
            captures_with_recaptures: self.watches.live_after_breaks(),
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
        self.engagement.job_found_at()
    }

    pub(super) fn found_job_at(&mut self, at: SystemTime) {
        self.engagement.found_job_at(at);
    }

    pub(super) fn age_job_start(&mut self, by: Duration) {
        self.engagement.age_job_start(by);
    }

    /// As a listing shows it busy, with no job named.
    pub(super) fn set_listed_busy(&mut self) {
        self.engagement.set_listed_busy();
    }

    pub(super) fn has_taken(&mut self, job: u64) {
        self.engagement.has_taken(job);
    }

    pub(super) fn set_key(&mut self, key: Key) {
        self.engagement.set_key(key);
    }

    /// Whether this daemon started it: a restart adopted it otherwise.
    pub(super) fn was_started_here(&self) -> bool {
        self.engagement.was_started_here()
    }
}
