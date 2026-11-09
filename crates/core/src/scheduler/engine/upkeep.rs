//! A profile's upkeep: whether a measure of slot 1 or a reseed runs, why a reseed is due, and when each last ran. It
//! owns that state and must not call Docker or decide what else upkeep waits for: `caches` does, and hands it the
//! task it started.

use std::collections::BTreeMap;
use std::time::SystemTime;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::caches::MEASURE_EVERY;
use crate::cache;
use crate::docker;

/// A measure of slot 1 running in the background.
pub(super) type MeasureTask = JoinHandle<Result<cache::Measured, docker::Error>>;

/// A reseed running in the background.
pub(super) type ReseedTask = JoinHandle<Result<String, docker::Error>>;

/// Why a profile is due to reseed, weakest first: a stronger reason replaces a weaker one and never the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Due {
    /// A measure found slot 1 changed `reseed_at` or more, or the last reseed unfinished, or could not tell.
    Changed,
    /// The last check of the caches found the snapshot over `snapshot_max`.
    Oversized,
    /// The API asked for it.
    Requested,
}

impl Due {
    /// Whether the reseed runs however little slot 1 changed.
    pub(super) fn forces(self) -> bool {
        self != Due::Changed
    }
}

/// The one background task a profile's upkeep may run: a measure reads the layers a reseed drops.
#[derive(Debug, Default)]
enum Activity {
    #[default]
    Idle,
    Measuring(MeasureTask),
    /// `memory` is the cap its seed container started with, which a reload does not change.
    Reseeding {
        task: ReseedTask,
        memory: u64,
    },
}

/// One profile's upkeep. `due` and `measure_due` are independent of `activity`: a reseed can be asked for during a
/// measure, and slot 1 can run a job during one.
#[derive(Debug, Default)]
struct ProfileUpkeep {
    activity: Activity,
    due: Option<Due>,
    /// Slot 1 ran a job since the last measure or reseed (ADR 0094).
    measure_due: bool,
    /// When the last measure started: the next waits `MEASURE_EVERY`.
    measured_at: Option<Instant>,
    /// When the last reseed finished: a check of the caches that started before then measured the old snapshot.
    reseeded_at: Option<SystemTime>,
}

impl ProfileUpkeep {
    fn mark(&mut self, reason: Due) {
        self.due = self.due.max(Some(reason));
    }

    fn slot_one_ran(&mut self) {
        self.measure_due = true;
    }

    fn take_due(&mut self) -> Option<Due> {
        self.measure_due = false;
        self.due.take()
    }

    fn measure_started(&mut self, task: MeasureTask, now: Instant) {
        self.measure_due = false;
        self.measured_at = Some(now);
        self.activity = Activity::Measuring(task);
    }

    fn reseed_started(&mut self, task: ReseedTask, memory: u64) {
        self.activity = Activity::Reseeding { task, memory };
    }

    fn take_finished_measure(&mut self) -> Option<MeasureTask> {
        match std::mem::take(&mut self.activity) {
            Activity::Measuring(task) if task.is_finished() => Some(task),
            other => {
                self.activity = other;
                None
            }
        }
    }

    fn take_finished_reseed(&mut self) -> Option<ReseedTask> {
        match std::mem::take(&mut self.activity) {
            Activity::Reseeding { task, .. } if task.is_finished() => {
                self.reseeded_at = Some(SystemTime::now());
                Some(task)
            }
            other => {
                self.activity = other;
                None
            }
        }
    }

    fn forget_due(&mut self) {
        self.due = None;
        self.measure_due = false;
    }
}

/// Every profile's upkeep (ADR 0116). A profile gets an entry the first time anything happens to it, and keeps it.
///
/// A restart keeps only a reseed whose seed container still runs, which recovery takes back; what was due and when
/// each last ran are lost, and slot 1 is measured again after its next job. A reload that removes a profile resets
/// what is due and keeps the entry: a measure or reseed running goes on, the removed profile retires only once that
/// reseed ends, and it keeps when each last ran should it come back.
#[derive(Debug, Default)]
pub(super) struct Upkeep {
    profiles: BTreeMap<String, ProfileUpkeep>,
}

impl Upkeep {
    fn profile(&mut self, profile: &str) -> &mut ProfileUpkeep {
        self.profiles.entry(profile.to_owned()).or_default()
    }

    /// The API asked to reseed `profile`.
    pub(super) fn request(&mut self, profile: &str) {
        self.mark(profile, Due::Requested);
    }

    /// `profile` is due to reseed for `reason`, unless already for a stronger one.
    pub(super) fn mark(&mut self, profile: &str, reason: Due) {
        self.profile(profile).mark(reason);
    }

    /// Slot 1 of `profile` ran a job.
    pub(super) fn slot_one_ran(&mut self, profile: &str) {
        self.profile(profile).slot_one_ran();
    }

    /// A reseed is starting: nothing stays due, and it weighs slot 1's changes itself. Returns why it was due.
    pub(super) fn take_due(&mut self, profile: &str) -> Option<Due> {
        self.profile(profile).take_due()
    }

    /// A measure of slot 1 started.
    pub(super) fn measure_started(&mut self, profile: &str, task: MeasureTask, now: Instant) {
        self.profile(profile).measure_started(task, now);
    }

    /// A reseed started with its seed container at `memory`, or recovery found one running.
    pub(super) fn reseed_started(&mut self, profile: &str, task: ReseedTask, memory: u64) {
        self.profile(profile).reseed_started(task, memory);
    }

    /// Takes the measures that ended, by profile.
    pub(super) fn finished_measures(&mut self) -> BTreeMap<String, MeasureTask> {
        let mut finished = BTreeMap::new();
        for (name, upkeep) in &mut self.profiles {
            if let Some(task) = upkeep.take_finished_measure() {
                finished.insert(name.clone(), task);
            }
        }
        finished
    }

    /// Takes the reseeds that ended, failed ones too, by profile.
    pub(super) fn finished_reseeds(&mut self) -> BTreeMap<String, ReseedTask> {
        let mut finished = BTreeMap::new();
        for (name, upkeep) in &mut self.profiles {
            if let Some(task) = upkeep.take_finished_reseed() {
                finished.insert(name.clone(), task);
            }
        }
        finished
    }

    /// A reload removed the profiles `kept` says no to: nothing stays due for them. The entry stays, with whatever
    /// runs and when each last ran.
    pub(super) fn forget_removed(&mut self, kept: impl Fn(&str) -> bool) {
        for (name, upkeep) in &mut self.profiles {
            if !kept(name) {
                upkeep.forget_due();
            }
        }
    }

    /// Why `profile` is due to reseed.
    pub(super) fn due(&self, profile: &str) -> Option<Due> {
        self.profiles.get(profile)?.due
    }

    /// The profiles due to reseed that nothing of their own upkeep holds back: one that measures slot 1 would read
    /// layers the reseed drops, and it ends in seconds.
    pub(super) fn due_to_reseed(&self) -> impl Iterator<Item = &String> {
        self.profiles
            .iter()
            .filter(|(_, upkeep)| upkeep.due.is_some() && matches!(upkeep.activity, Activity::Idle))
            .map(|(name, _)| name)
    }

    /// The profiles whose slot 1 is due a measure at `now`: not one already due to reseed, which weighs slot 1's
    /// changes itself, nor one with a task running, nor within `MEASURE_EVERY` of its last.
    pub(super) fn due_to_measure(&self, now: Instant) -> impl Iterator<Item = &String> {
        self.profiles
            .iter()
            .filter(move |(_, upkeep)| {
                upkeep.measure_due
                    && upkeep.due.is_none()
                    && matches!(upkeep.activity, Activity::Idle)
                    && upkeep
                        .measured_at
                        .is_none_or(|at| now.duration_since(at) >= MEASURE_EVERY)
            })
            .map(|(name, _)| name)
    }

    /// The profiles the API asked to reseed that have not started.
    pub(super) fn requested(&self) -> impl Iterator<Item = &String> {
        self.profiles
            .iter()
            .filter(|(_, upkeep)| upkeep.due == Some(Due::Requested))
            .map(|(name, _)| name)
    }

    /// Whether the API asked to reseed `profile` and that has not started.
    pub(super) fn is_requested(&self, profile: &str) -> bool {
        self.due(profile) == Some(Due::Requested)
    }

    /// Whether a reseed of `profile` runs.
    pub(super) fn reseed_runs(&self, profile: &str) -> bool {
        self.profiles
            .get(profile)
            .is_some_and(|upkeep| matches!(upkeep.activity, Activity::Reseeding { .. }))
    }

    /// Whether a reseed of `profile` runs, or will once its slots are free.
    pub(super) fn reseed_runs_or_is_requested(&self, profile: &str) -> bool {
        self.reseed_runs(profile) || self.is_requested(profile)
    }

    /// Whether a requested reseed of `profile` has yet to start.
    pub(super) fn requested_reseed_waits(&self, profile: &str) -> bool {
        self.is_requested(profile) && !self.reseed_runs(profile)
    }

    /// Whether a check of the caches that started at `check_began` measured `profile`'s present snapshot: no reseed
    /// runs that may have replaced it since, and none finished since.
    pub(super) fn snapshot_unchanged_since(&self, profile: &str, check_began: SystemTime) -> bool {
        !self.reseed_runs(profile)
            && self
                .profiles
                .get(profile)
                .and_then(|upkeep| upkeep.reseeded_at)
                .is_none_or(|at| at < check_began)
    }

    /// The memory cap of each seed container running, by profile.
    pub(super) fn seed_memories(&self) -> BTreeMap<String, u64> {
        self.profiles
            .iter()
            .filter_map(|(name, upkeep)| match upkeep.activity {
                Activity::Reseeding { memory, .. } => Some((name.clone(), memory)),
                Activity::Idle | Activity::Measuring(_) => None,
            })
            .collect()
    }

    /// The memory caps of the seed containers running, together.
    pub(super) fn seeds_memory(&self) -> u64 {
        self.seed_memories().values().sum()
    }

    #[cfg(test)]
    pub(super) fn measure_is_due(&self, profile: &str) -> bool {
        self.profiles
            .get(profile)
            .is_some_and(|upkeep| upkeep.measure_due)
    }

    /// How many profiles are due because their snapshot is over its limit.
    #[cfg(test)]
    pub(super) fn oversized(&self) -> usize {
        self.profiles
            .values()
            .filter(|upkeep| upkeep.due == Some(Due::Oversized))
            .count()
    }
}
