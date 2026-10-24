//! Which runners to start and stop, as a pure function of queued jobs, slots and the memory and CPU budgets (`plan`), and
//! the loop that applies those decisions with GitHub and Docker (`engine`). `plan` does no I/O and reads no clock.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use crate::api::Waiting;
use crate::github::QueuedJob;

pub mod engine;

/// How long a started runner may wait for a job before it counts as surplus: covers the container's start and
/// GitHub's assignment.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Once a job has waited this long for CPU, no younger job takes CPU until it fits: smaller jobs arriving all the
/// time would otherwise take every CPU that frees up before it.
pub const CPU_PRIORITY_AFTER: Duration = Duration::from_secs(120);

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
    /// What a new runner of it takes of the budget: `None`, its profile's `memory`. Under measured admission, the
    /// largest predicted peak of its queued jobs, since GitHub may give the runner any of them.
    pub runner_memory: Option<u64>,
    /// What a new runner of it takes of the CPU budget, in millicores: `None`, its profile's `cpus`. Under measured
    /// admission, the largest predicted average CPU of its queued jobs.
    pub runner_cpu: Option<u64>,
    /// Since when its next job waits for CPU that a slot and memory have room for.
    pub cpu_waiting_since: Option<SystemTime>,
}

impl Repository {
    /// Whether its next job has waited `CPU_PRIORITY_AFTER` or more for CPU at `now`.
    fn starved_of_cpu(&self, now: SystemTime) -> bool {
        self.cpu_waiting_since
            .and_then(|since| now.duration_since(since).ok())
            .is_some_and(|waited| waited >= CPU_PRIORITY_AFTER)
    }
}

/// A profile's slots: one concurrent job each.
#[derive(Debug, Clone)]
pub struct Profile {
    /// Memory a slot's container may use.
    pub memory: u64,
    /// CPU a slot's container may use, in millicores.
    pub cpus: u64,
    /// While the profile's snapshot is rebuilt or its image pulled, none of its slots starts.
    pub preparing: Option<Preparing>,
    /// Index 0 is slot 1.
    pub slots: Vec<Slot>,
}

/// Why none of a profile's slots starts for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preparing {
    /// Slot 1's layers become the profile's snapshot.
    Reseeding,
    /// Its image is being pulled.
    PullingImage,
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
        /// What it takes of the budget.
        memory: u64,
        /// What it takes of the CPU budget, in millicores.
        cpu: u64,
    },
    /// Hold the repository's next job: a slot and memory had room for its runner, the CPU budget did not.
    HeldForCpu {
        /// `owner/name`.
        repository: String,
    },
    /// Remove the slot's runner, which waited `IDLE_TIMEOUT` for a job no longer queued.
    Stop {
        /// The slot whose runner stops.
        slot: SlotRef,
    },
}

/// What `plan` decides, and why each queued job it starts no runner for waits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// What to start, hold and stop.
    pub decisions: Vec<Decision>,
    /// Why each queued job of a repository's matching jobs waits, by job ID. A job a runner starts for, or an idle
    /// runner will take, is `Waiting::RunnerStarting`.
    pub waiting: BTreeMap<u64, Waiting>,
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

/// Decides which slots start a runner and which idle runners stop, and why each job left queued waits.
///
/// Each repository gets a runner per matching queued job, less its runners still waiting for a job. Jobs are
/// served oldest first across repositories, each in the lowest free slot of its repository's profile, while what
/// the runner takes (`Repository::runner_memory`, by default the slot's memory) fits `free_memory` and its CPU
/// (`Repository::runner_cpu`, by default the slot's `cpus`) fits `free_cpu` (`None`: no budget). A repository whose
/// job had a slot and memory but not CPU is held once (`Decision::HeldForCpu`). An idle runner stops once no queued
/// job is left for it and it waited `IDLE_TIMEOUT`. Once a held repository waited `CPU_PRIORITY_AFTER`, no younger
/// job starts until its own fits.
pub fn plan(
    repositories: &[Repository],
    profiles: &BTreeMap<String, Profile>,
    free_memory: Option<u64>,
    free_cpu: Option<u64>,
    now: SystemTime,
) -> Plan {
    let mut decided = Plan::default();
    let mut wanted = BTreeMap::new();
    let mut jobs = Vec::new();
    for repository in repositories {
        if let Some(serve) = sort_out(repository, profiles, now, &mut decided) {
            wanted.insert(repository.name.as_str(), serve.len());
            jobs.extend(serve.into_iter().map(|job| (job, repository)));
        }
    }
    jobs.sort_by_key(|(id, _)| *id);

    let mut taken = taken_slots(profiles);
    let mut free_memory = free_memory;
    let mut free_cpu = free_cpu;
    // The job that waited long enough for CPU that no younger one takes CPU before it.
    let mut first_for_cpu = None;
    for (job, repository) in jobs {
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
        if let Some(older) = first_for_cpu {
            decided
                .waiting
                .insert(job, Waiting::BehindCpu { job: older });
            continue;
        }
        let profile_name = || repository.profile.clone();
        let Some(index) = taken.iter().position(|taken| !taken) else {
            let reason = match profile.preparing {
                Some(Preparing::Reseeding) => Waiting::Reseeding {
                    profile: profile_name(),
                },
                Some(Preparing::PullingImage) => Waiting::PullingImage {
                    profile: profile_name(),
                },
                None => Waiting::NoSlot {
                    profile: profile_name(),
                },
            };
            decided.waiting.insert(job, reason);
            continue;
        };
        let memory = repository.runner_memory.unwrap_or(profile.memory);
        if let Some(free) = free_memory.filter(|free| *free < memory) {
            decided.waiting.insert(
                job,
                Waiting::Memory {
                    needs: memory,
                    free,
                },
            );
            continue;
        }
        let cpu = repository.runner_cpu.unwrap_or(profile.cpus);
        if let Some(free) = free_cpu.filter(|free| *free < cpu) {
            let held = Decision::HeldForCpu {
                repository: repository.name.clone(),
            };
            if !decided.decisions.contains(&held) {
                decided.decisions.push(held);
            }
            decided
                .waiting
                .insert(job, Waiting::Cpu { needs: cpu, free });
            // It waited long enough: no younger job takes CPU until it fits.
            if repository.starved_of_cpu(now) {
                first_for_cpu = Some(job);
            }
            continue;
        }
        taken[index] = true;
        *missing -= 1;
        free_memory = free_memory.map(|free| free - memory);
        free_cpu = free_cpu.map(|free| free - cpu);
        decided.waiting.insert(job, Waiting::RunnerStarting);
        decided.decisions.push(Decision::Start {
            slot: SlotRef {
                profile: repository.profile.clone(),
                index,
            },
            repository: repository.name.clone(),
            memory,
            cpu,
        });
    }
    decided
}

/// Stops a repository's idle runners no job is left for and says why its jobs wait when it is paused or held, or
/// which an idle runner will take. Returns the IDs of the jobs it wants a runner for, unless it is held.
fn sort_out(
    repository: &Repository,
    profiles: &BTreeMap<String, Profile>,
    now: SystemTime,
    decided: &mut Plan,
) -> Option<Vec<u64>> {
    let mut matching: Vec<&QueuedJob> = repository
        .queued
        .iter()
        .filter(|job| matches(job, &repository.labels))
        .collect();
    if repository.paused {
        decided
            .waiting
            .extend(matching.iter().map(|job| (job.id, Waiting::Paused)));
        // None is left for its idle runners, which stop.
        matching.clear();
    }
    let idle: Vec<(SlotRef, &RunnerSlot)> = profiles
        .get(&repository.profile)
        .into_iter()
        .flat_map(|profile| profile.slots.iter().enumerate())
        .filter_map(|(index, slot)| match slot {
            Slot::Runner(runner) if runner.repository == repository.name && !runner.busy => Some((
                SlotRef {
                    profile: repository.profile.clone(),
                    index,
                },
                runner,
            )),
            _ => None,
        })
        .collect();
    let surplus = idle.len().saturating_sub(matching.len());
    decided.decisions.extend(
        idle.iter()
            .filter(|(_, runner)| {
                now.duration_since(runner.started_at)
                    .is_ok_and(|waited| waited >= IDLE_TIMEOUT)
            })
            .take(surplus)
            .map(|(slot, _)| Decision::Stop { slot: slot.clone() }),
    );
    let missing = matching.len().saturating_sub(idle.len());
    // The oldest jobs are the ones still waiting once the idle runners take theirs.
    let skip = matching.len() - missing;
    decided.waiting.extend(
        matching
            .iter()
            .take(skip)
            .map(|job| (job.id, Waiting::RunnerStarting)),
    );
    let serve = matching.iter().skip(skip).map(|job| job.id);
    match repository.held_until.filter(|until| *until > now) {
        Some(until) => {
            let until = crate::time::ms(until) / 1000;
            decided
                .waiting
                .extend(serve.map(|job| (job, Waiting::Backoff { until })));
            None
        }
        None => Some(serve.collect()),
    }
}

/// Whether each slot of each profile is taken, by profile name: by a runner, closed, or its profile preparing.
fn taken_slots(profiles: &BTreeMap<String, Profile>) -> BTreeMap<&str, Vec<bool>> {
    profiles
        .iter()
        .map(|(name, profile)| {
            let taken = profile
                .slots
                .iter()
                .map(|slot| profile.preparing.is_some() || *slot != Slot::Free)
                .collect();
            (name.as_str(), taken)
        })
        .collect()
}

#[cfg(test)]
mod tests;
