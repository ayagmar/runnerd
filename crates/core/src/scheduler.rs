//! Which runners to start and stop, as a pure function of queued jobs, slots and the memory budget (`plan`), and
//! the loop that applies those decisions with GitHub and Docker (`engine`). `plan` does no I/O and reads no clock.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use crate::github::QueuedJob;

pub mod engine;

/// How long a started runner may wait for a job before it counts as surplus: covers the container's start and
/// GitHub's assignment.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// A repository as the planner sees it.
#[derive(Debug, Clone)]
pub struct Repository {
    /// `owner/name` on GitHub.
    pub name: String,
    /// The profile its jobs run in.
    pub profile: String,
    /// The labels its runners register with.
    pub labels: Vec<String>,
    /// Paused repositories get no new runners; their running jobs finish.
    pub paused: bool,
    /// No runner starts for this repository before then, after runners failed to take a job.
    pub held_until: Option<SystemTime>,
    /// Its queued jobs at the last poll.
    pub queued: Vec<QueuedJob>,
}

/// A profile's slots: one concurrent job each.
#[derive(Debug, Clone)]
pub struct Profile {
    /// Memory a slot's container may use.
    pub memory: u64,
    /// While the profile's snapshot is rebuilt, none of its slots starts.
    pub reseeding: bool,
    /// Index 0 is slot 1.
    pub slots: Vec<Slot>,
}

/// What a slot holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// Nothing: a runner may start here.
    Free,
    /// A runner, waiting for its job or running it.
    Runner(RunnerSlot),
    /// Free, but over the profile's number of slots, which a reload lowered while its runners ran: nothing starts
    /// here.
    Closed,
}

/// A slot whose container runs a runner registered for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSlot {
    /// `owner/name` the runner is registered on.
    pub repository: String,
    /// Whether GitHub reports the runner running a job.
    pub busy: bool,
    /// When its container started.
    pub started_at: SystemTime,
}

/// Where slots are, by profile name and slot index (0 is slot 1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotRef {
    /// The profile's name.
    pub profile: String,
    /// 0 is slot 1.
    pub index: usize,
}

/// What `plan` decides for one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Register a runner for the repository and start it in the slot.
    Start {
        /// The free slot.
        slot: SlotRef,
        /// `owner/name` the runner registers on.
        repository: String,
    },
    /// Remove the slot's runner, which waited `IDLE_TIMEOUT` for a job no longer queued.
    Stop {
        /// The slot whose runner stops.
        slot: SlotRef,
    },
}

/// Whether a job may run on a repository's runners: every label it asks for is one of theirs. GitHub compares
/// labels without regard to case.
pub fn matches(job: &QueuedJob, labels: &[String]) -> bool {
    job.labels.iter().all(|wanted| {
        labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(wanted))
    })
}

/// Decides which slots start a runner and which idle runners stop.
///
/// Each repository gets a runner per matching queued job, less its runners still waiting for a job. Jobs are
/// served oldest first across repositories, each in the lowest free slot of its repository's profile, while the
/// slot's memory fits `free_memory` (`None`: no budget). An idle runner stops once no queued job is left for it and
/// it waited `IDLE_TIMEOUT`.
pub fn plan(
    repositories: &[Repository],
    profiles: &BTreeMap<String, Profile>,
    free_memory: Option<u64>,
    now: SystemTime,
) -> Vec<Decision> {
    let mut decisions = Vec::new();
    let mut wanted = BTreeMap::new();
    let mut jobs = Vec::new();
    for repository in repositories {
        let held = repository.held_until.is_some_and(|until| until > now);
        let matching: Vec<&QueuedJob> = if repository.paused {
            Vec::new()
        } else {
            repository
                .queued
                .iter()
                .filter(|job| matches(job, &repository.labels))
                .collect()
        };
        let idle: Vec<(SlotRef, &RunnerSlot)> = profiles
            .get(&repository.profile)
            .into_iter()
            .flat_map(|profile| profile.slots.iter().enumerate())
            .filter_map(|(index, slot)| match slot {
                Slot::Runner(runner) if runner.repository == repository.name && !runner.busy => {
                    Some((
                        SlotRef {
                            profile: repository.profile.clone(),
                            index,
                        },
                        runner,
                    ))
                }
                _ => None,
            })
            .collect();
        let surplus = idle.len().saturating_sub(matching.len());
        decisions.extend(
            idle.iter()
                .filter(|(_, runner)| {
                    now.duration_since(runner.started_at)
                        .is_ok_and(|waited| waited >= IDLE_TIMEOUT)
                })
                .take(surplus)
                .map(|(slot, _)| Decision::Stop { slot: slot.clone() }),
        );
        if !held {
            let missing = matching.len().saturating_sub(idle.len());
            wanted.insert(repository.name.as_str(), missing);
            // The oldest jobs are the ones still waiting once the idle runners take theirs.
            let skip = matching.len() - missing;
            jobs.extend(matching.iter().skip(skip).map(|job| (job.id, repository)));
        }
    }
    jobs.sort_by_key(|(id, _)| *id);

    let mut taken: BTreeMap<&str, Vec<bool>> = profiles
        .iter()
        .map(|(name, profile)| {
            let taken = profile
                .slots
                .iter()
                .map(|slot| profile.reseeding || *slot != Slot::Free)
                .collect();
            (name.as_str(), taken)
        })
        .collect();
    let mut free_memory = free_memory;
    for (_, repository) in jobs {
        let Some(missing) = wanted.get_mut(repository.name.as_str()) else {
            continue;
        };
        if *missing == 0 {
            continue;
        }
        let (Some(profile), Some(taken)) = (
            profiles.get(&repository.profile),
            taken.get_mut(repository.profile.as_str()),
        ) else {
            continue;
        };
        if free_memory.is_some_and(|free| free < profile.memory) {
            continue;
        }
        let Some(index) = taken.iter().position(|taken| !taken) else {
            continue;
        };
        taken[index] = true;
        *missing -= 1;
        free_memory = free_memory.map(|free| free - profile.memory);
        decisions.push(Decision::Start {
            slot: SlotRef {
                profile: repository.profile.clone(),
                index,
            },
            repository: repository.name.clone(),
        });
    }
    decisions
}

#[cfg(test)]
mod tests;
