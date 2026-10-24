//! Which runners to start and stop, as a pure function of queued jobs, slots, warm runners and the memory and CPU
//! budgets (`plan`), how many warm runners each repository wants (`warm`), and the loop that applies those decisions
//! with GitHub and Docker (`engine`). `plan` and `warm` do no I/O and read no clock.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use crate::api::Waiting;
use crate::github::QueuedJob;

pub mod engine;
pub mod warm;

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
    /// How many idle runners it keeps beyond its queued jobs, for jobs not queued yet (`warm::target`).
    pub warm: usize,
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
    /// CPU a slot's container may use, in millicores; under a CPU budget, at most the budget, as a runner at its caps
    /// reserves it.
    pub cpus: u64,
    /// While the profile's snapshot is rebuilt or its image pulled, none of its slots starts.
    pub preparing: Option<Preparing>,
    /// Upkeep waits for its runners, all idle: its reseed (slot 1 ran a job since the last), or the shared caches'
    /// check, which waits for every profile's. Its warm runners stop, and none starts, so the upkeep can run.
    pub upkeep_due: bool,
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
    /// What it takes of the memory budget, which it gives back if it stops.
    pub memory: u64,
    /// What it takes of the CPU budget, in millicores.
    pub cpu: u64,
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
        /// Whether it starts for no queued job, as a warm runner.
        warm: bool,
        /// The warm runners this plan stops to make room for it, just before: if one is still there, it took a job
        /// meanwhile and the start waits for the next plan.
        replacing: Vec<SlotRef>,
    },
    /// Hold the repository's next job: a slot and memory had room for its runner, the CPU budget did not.
    HeldForCpu {
        /// `owner/name`.
        repository: String,
    },
    /// Remove the slot's runner: it waited `IDLE_TIMEOUT` for a job no longer queued and is not kept warm, or it is a
    /// warm runner whose room a queued job takes.
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
    /// Why each queued job of a repository's matching jobs waits, by job ID. A job an idle runner will take is
    /// `Waiting::RunnerStarting`; a job this plan starts a runner for has no reason, as that runner does not exist
    /// until the start succeeds.
    pub waiting: BTreeMap<u64, Waiting>,
    /// The idle runners kept warm: no queued job of their repository waits for them.
    pub warm: BTreeSet<SlotRef>,
}

/// The budgets a plan starts runners within.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budget {
    /// The memory left; `None` for no budget.
    pub memory: Option<u64>,
    /// The CPU left, in millicores; `None` for no budget.
    pub cpu: Option<u64>,
    /// Measured admission holds the slice at its high-water mark: nothing starts, so no warm runner stops to make
    /// room.
    pub held: bool,
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
/// the runner takes (`Repository::runner_memory`, by default the slot's memory) fits `budget.memory` and its CPU
/// (`Repository::runner_cpu`, by default the slot's `cpus`) fits `budget.cpu` (`None`: no budget). A job that does
/// not fit takes the room of warm runners of other repositories, which stop. A repository whose job had a slot and
/// memory but not CPU is held once (`Decision::HeldForCpu`). An idle runner stops once no queued job is left for it,
/// it is not one of its repository's `warm` runners, and it waited `IDLE_TIMEOUT`. Once a held repository waited
/// `CPU_PRIORITY_AFTER`, no younger job starts until its own fits. Last, while no job waits for room, each
/// repository short of its `warm` runners starts them at its profile's caps, one per repository at a time.
pub fn plan(
    repositories: &[Repository],
    profiles: &BTreeMap<String, Profile>,
    budget: Budget,
    now: SystemTime,
) -> Plan {
    let mut decided = Plan::default();
    let mut wanted = BTreeMap::new();
    let mut warm_wanted = Vec::new();
    let mut jobs = Vec::new();
    for repository in repositories {
        if let Some(sorted) = sort_out(repository, profiles, now, &mut decided) {
            wanted.insert(repository.name.as_str(), sorted.serve.len());
            jobs.extend(sorted.serve.into_iter().map(|job| (job, repository)));
            warm_wanted.push((repository, sorted.warm));
        }
    }
    jobs.sort_by_key(|(id, _)| *id);

    let mut warm = Warm::new(&decided.warm, profiles, budget.held);
    let mut taken = taken_slots(profiles);
    let mut started = Started::new(&taken, budget.memory, budget.cpu);
    let mut left = Left {
        memory: budget.memory,
        cpu: budget.cpu,
    };
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
        let memory = repository.runner_memory.unwrap_or(profile.memory);
        let cpu = repository.runner_cpu.unwrap_or(profile.cpus);
        let free_index = taken.iter().position(|taken| !taken);
        let fits = free_index.is_some() && left.fits(memory, cpu);
        let room = if fits || profile.preparing.is_some() {
            None
        } else {
            warm.make_room(repository, free_index.is_none(), left.short(memory, cpu))
        };
        let index = match (room.as_ref(), free_index) {
            (Some(room), free) => free.or(room.slot).unwrap_or_default(),
            (None, Some(index)) if fits => index,
            (None, free) => {
                let (reason, short) = why_waits(
                    &repository.profile,
                    profile,
                    free.is_some(),
                    memory,
                    cpu,
                    left,
                    &started,
                );
                decided.waiting.insert(job, reason);
                if short == Short::Cpu {
                    let held = Decision::HeldForCpu {
                        repository: repository.name.clone(),
                    };
                    if !decided.decisions.contains(&held) {
                        decided.decisions.push(held);
                    }
                    // It waited long enough: no younger job takes CPU until it fits.
                    if repository.starved_of_cpu(now) {
                        first_for_cpu = Some(job);
                    }
                }
                continue;
            }
        };
        // The warm runners whose room it takes stop first.
        let replacing = room.map_or_else(Vec::new, |room| {
            left.give(room.memory, room.cpu);
            for slot in &room.stopped {
                decided.warm.remove(slot);
                decided
                    .decisions
                    .push(Decision::Stop { slot: slot.clone() });
            }
            room.stopped
        });
        taken[index] = true;
        *missing -= 1;
        started.note(&repository.profile, job);
        left.take(memory, cpu);
        decided
            .decisions
            .push(start(repository, index, memory, cpu, replacing));
    }
    if !budget.held {
        start_warm(&warm_wanted, profiles, &mut taken, left, &mut decided);
    }
    decided
}

/// What a job's runner lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Short {
    Slot,
    Memory,
    Cpu,
}

/// Why a job whose runner of `memory` and `cpu` does not fit in `profile` (named `name`) waits, given whether it has
/// a `free_slot` and the budgets `left`, and what it lacks.
fn why_waits(
    name: &str,
    profile: &Profile,
    free_slot: bool,
    memory: u64,
    cpu: u64,
    left: Left,
    started: &Started<'_>,
) -> (Waiting, Short) {
    let profile_name = || name.to_owned();
    if !free_slot {
        let reason = match profile.preparing {
            Some(Preparing::Reseeding) => Waiting::Reseeding {
                profile: profile_name(),
            },
            Some(Preparing::PullingImage) => Waiting::PullingImage {
                profile: profile_name(),
            },
            None => started.for_slot(
                name,
                Waiting::NoSlot {
                    profile: profile_name(),
                },
            ),
        };
        return (reason, Short::Slot);
    }
    match (left.memory, left.cpu) {
        (Some(free), _) if free < memory => (
            started.for_memory(
                memory,
                Waiting::Memory {
                    needs: memory,
                    free,
                },
            ),
            Short::Memory,
        ),
        (_, Some(free)) => (
            started.for_cpu(cpu, Waiting::Cpu { needs: cpu, free }),
            Short::Cpu,
        ),
        // `fits` said otherwise.
        (_, None) => (
            Waiting::NoSlot {
                profile: profile_name(),
            },
            Short::Slot,
        ),
    }
}

/// A runner of `repository` in slot `index` of its profile for a queued job, after the warm runners it `replaces`
/// stop.
fn start(
    repository: &Repository,
    index: usize,
    memory: u64,
    cpu: u64,
    replacing: Vec<SlotRef>,
) -> Decision {
    Decision::Start {
        slot: SlotRef {
            profile: repository.profile.clone(),
            index,
        },
        repository: repository.name.clone(),
        memory,
        cpu,
        warm: false,
        replacing,
    }
}

/// Starts the warm runners each repository is short of (`wanted`), at its profile's caps, in the slots and budget
/// the jobs `left`: one per repository at a time, so a few free slots go to several repositories rather than to the
/// first. None while a job waits for room they would take.
fn start_warm(
    wanted: &[(&Repository, usize)],
    profiles: &BTreeMap<String, Profile>,
    taken: &mut BTreeMap<&str, Vec<bool>>,
    mut left: Left,
    decided: &mut Plan,
) {
    let room_wanted = decided.waiting.values().any(|reason| {
        matches!(
            reason,
            Waiting::NoSlot { .. }
                | Waiting::Memory { .. }
                | Waiting::Cpu { .. }
                | Waiting::BehindCpu { .. }
                | Waiting::BehindStart { .. }
        )
    });
    if room_wanted {
        return;
    }
    for round in 0.. {
        let mut any = false;
        for (repository, wants) in wanted {
            if *wants <= round {
                continue;
            }
            let (Some(profile), Some(taken)) = (
                profiles.get(&repository.profile),
                taken.get_mut(repository.profile.as_str()),
            ) else {
                continue;
            };
            let (memory, cpu) = (profile.memory, profile.cpus);
            let Some(index) = taken.iter().position(|taken| !taken) else {
                continue;
            };
            if !left.fits(memory, cpu) {
                continue;
            }
            taken[index] = true;
            left.take(memory, cpu);
            any = true;
            decided.decisions.push(Decision::Start {
                slot: SlotRef {
                    profile: repository.profile.clone(),
                    index,
                },
                repository: repository.name.clone(),
                memory,
                cpu,
                warm: true,
                replacing: Vec::new(),
            });
        }
        if !any {
            break;
        }
    }
}

/// The memory and CPU budgets a plan has left; `None`, no budget.
#[derive(Debug, Clone, Copy)]
struct Left {
    memory: Option<u64>,
    cpu: Option<u64>,
}

impl Left {
    fn fits(self, memory: u64, cpu: u64) -> bool {
        self.memory.is_none_or(|free| free >= memory) && self.cpu.is_none_or(|free| free >= cpu)
    }

    /// What a runner of `memory` and `cpu` lacks: none where there is no budget.
    fn short(self, memory: u64, cpu: u64) -> Left {
        Left {
            memory: self.memory.map(|free| memory.saturating_sub(free)),
            cpu: self.cpu.map(|free| cpu.saturating_sub(free)),
        }
    }

    fn take(&mut self, memory: u64, cpu: u64) {
        self.memory = self.memory.map(|free| free.saturating_sub(memory));
        self.cpu = self.cpu.map(|free| free.saturating_sub(cpu));
    }

    fn give(&mut self, memory: u64, cpu: u64) {
        self.memory = self.memory.map(|free| free + memory);
        self.cpu = self.cpu.map(|free| free + cpu);
    }
}

/// What `sort_out` leaves a repository to start: runners for the IDs of its jobs, and warm runners.
struct Sorted {
    serve: Vec<u64>,
    warm: usize,
}

/// Stops a repository's idle runners no job is left for, beyond its warm ones, and says why its jobs wait when it is
/// paused or held, or which an idle runner will take. Returns the jobs it wants a runner for and how many warm
/// runners it is short of, unless it is held.
fn sort_out(
    repository: &Repository,
    profiles: &BTreeMap<String, Profile>,
    now: SystemTime,
    decided: &mut Plan,
) -> Option<Sorted> {
    let mut matching: Vec<&QueuedJob> = repository
        .queued
        .iter()
        .filter(|job| matches(job, &repository.labels))
        .collect();
    let profile = profiles.get(&repository.profile);
    let mut warm = repository.warm;
    if repository.paused {
        decided
            .waiting
            .extend(matching.iter().map(|job| (job.id, Waiting::Paused)));
        // None is left for its idle runners, which stop.
        matching.clear();
        warm = 0;
    }
    // Upkeep waits for its profile's runners: none is kept, and none waits out `IDLE_TIMEOUT` first.
    let upkeep_due = profile.is_some_and(|profile| profile.upkeep_due);
    if upkeep_due {
        warm = 0;
    }
    let idle: Vec<(SlotRef, &RunnerSlot)> = profile
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
    let stops: Vec<SlotRef> = idle
        .iter()
        .filter(|(_, runner)| {
            upkeep_due
                || now
                    .duration_since(runner.started_at)
                    .is_ok_and(|waited| waited >= IDLE_TIMEOUT)
        })
        .take(surplus.saturating_sub(warm))
        .map(|(slot, _)| slot.clone())
        .collect();
    // The idle runners left past those its jobs take are warm.
    let kept: Vec<&SlotRef> = idle
        .iter()
        .map(|(slot, _)| slot)
        .filter(|slot| !stops.contains(slot))
        .collect();
    decided
        .warm
        .extend(kept.iter().skip(matching.len()).map(|slot| (*slot).clone()));
    let warm = warm.saturating_sub(kept.len().saturating_sub(matching.len()));
    decided
        .decisions
        .extend(stops.into_iter().map(|slot| Decision::Stop { slot }));
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
        None => Some(Sorted {
            serve: serve.collect(),
            warm,
        }),
    }
}

/// The warm runners a queued job may stop to make room for itself.
struct Warm<'plan> {
    /// Each warm runner with what it takes, in slot order; `None` once stopped for a job.
    runners: Vec<Option<(SlotRef, &'plan RunnerSlot)>>,
}

/// The room some warm runners give back.
struct Room {
    /// The warm runners to stop.
    stopped: Vec<SlotRef>,
    /// A slot of the job's profile among them, when it needed one.
    slot: Option<usize>,
    memory: u64,
    cpu: u64,
}

impl<'plan> Warm<'plan> {
    /// The warm runners of `warm`; none while the slice is `held`, when no start may use their room.
    fn new(
        warm: &BTreeSet<SlotRef>,
        profiles: &'plan BTreeMap<String, Profile>,
        held: bool,
    ) -> Warm<'plan> {
        let runners = warm
            .iter()
            .filter(|_| !held)
            .filter_map(
                |slot| match profiles.get(&slot.profile)?.slots.get(slot.index)? {
                    Slot::Runner(runner) => Some(Some((slot.clone(), runner))),
                    Slot::Free | Slot::Closed => None,
                },
            )
            .collect();
        Warm { runners }
    }

    /// Picks warm runners of repositories other than `repository` whose room lets its next runner start: a slot of
    /// its profile when it has none (`slot`), and the memory and CPU it is `short` of. Those in its profile go first.
    /// `None`, and nothing picked, when all of them would not do.
    fn make_room(&mut self, repository: &Repository, slot: bool, short: Left) -> Option<Room> {
        let (memory, cpu) = (short.memory.unwrap_or(0), short.cpu.unwrap_or(0));
        let mut candidates: Vec<usize> = self
            .runners
            .iter()
            .enumerate()
            .filter_map(|(at, runner)| {
                runner
                    .as_ref()
                    .filter(|(_, runner)| runner.repository != repository.name)
                    .map(|_| at)
            })
            .collect();
        // Its own profile's first: their slots serve it too.
        candidates.sort_by_key(|at| {
            self.runners[*at]
                .as_ref()
                .is_some_and(|(slot, _)| slot.profile != repository.profile)
        });
        let mut room = Room {
            stopped: Vec::new(),
            slot: None,
            memory: 0,
            cpu: 0,
        };
        let mut picked = Vec::new();
        for at in candidates {
            let short = (slot && room.slot.is_none()) || room.memory < memory || room.cpu < cpu;
            if !short {
                break;
            }
            let Some((ref_, runner)) = &self.runners[at] else {
                continue;
            };
            let gives_slot = ref_.profile == repository.profile;
            // A runner that gives nothing the job still lacks stays.
            let helps = (gives_slot && slot && room.slot.is_none())
                || (room.memory < memory && runner.memory > 0)
                || (room.cpu < cpu && runner.cpu > 0);
            if !helps {
                continue;
            }
            if gives_slot && room.slot.is_none() {
                room.slot = Some(ref_.index);
            }
            room.memory += runner.memory;
            room.cpu += runner.cpu;
            room.stopped.push(ref_.clone());
            picked.push(at);
        }
        if (slot && room.slot.is_none()) || room.memory < memory || room.cpu < cpu {
            return None;
        }
        for at in picked {
            self.runners[at] = None;
        }
        Some(room)
    }
}

/// What a plan's own starts took, so a job held only by them waits behind them rather than for a slot, memory or CPU
/// in use. The status explains with a plan it does not run, whose starts may never happen.
struct Started<'plan> {
    /// The profiles with a free slot before the plan's starts.
    free_slots: BTreeSet<&'plan str>,
    /// The memory and CPU budgets left before them.
    memory: Option<u64>,
    cpu: Option<u64>,
    /// The first job each profile, and any profile, starts a runner for.
    by_profile: BTreeMap<&'plan str, u64>,
    first: Option<u64>,
}

impl<'plan> Started<'plan> {
    fn new(
        taken: &BTreeMap<&'plan str, Vec<bool>>,
        memory: Option<u64>,
        cpu: Option<u64>,
    ) -> Started<'plan> {
        Started {
            free_slots: taken
                .iter()
                .filter(|(_, slots)| slots.contains(&false))
                .map(|(name, _)| *name)
                .collect(),
            memory,
            cpu,
            by_profile: BTreeMap::new(),
            first: None,
        }
    }

    /// The plan starts a runner for `job` in `profile`.
    fn note(&mut self, profile: &'plan str, job: u64) {
        self.by_profile.entry(profile).or_insert(job);
        self.first.get_or_insert(job);
    }

    /// `reason` for a job `profile` has no slot for, unless only this plan's starts took its free slots.
    fn for_slot(&self, profile: &str, reason: Waiting) -> Waiting {
        match self.by_profile.get(profile) {
            Some(older) if self.free_slots.contains(profile) => {
                Waiting::BehindStart { job: *older }
            }
            _ => reason,
        }
    }

    /// `reason` for a job whose `needs` of memory do not fit, unless they did before this plan's starts.
    fn for_memory(&self, needs: u64, reason: Waiting) -> Waiting {
        Self::behind(self.first, self.memory, needs).unwrap_or(reason)
    }

    /// `reason` for a job whose `needs` of CPU do not fit, unless they did before this plan's starts.
    fn for_cpu(&self, needs: u64, reason: Waiting) -> Waiting {
        Self::behind(self.first, self.cpu, needs).unwrap_or(reason)
    }

    fn behind(first: Option<u64>, free: Option<u64>, needs: u64) -> Option<Waiting> {
        first
            .filter(|_| free.is_some_and(|free| free >= needs))
            .map(|job| Waiting::BehindStart { job })
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
