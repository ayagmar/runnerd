//! Auto sizing: which profiles' `memory`, `slots` and `cpus` the daemon changes now, from the advice, the runs since
//! each profile's last change and the limits `[sizing]` and each profile set. Pure: the daemon reads the history and
//! when each profile last changed, writes each change through the config check as `PUT /v1/config` does, and records
//! it. Never lowers a profile a run met the cap of, never changes `memory` from fewer than `[sizing] min_jobs` jobs,
//! never lowers within `[sizing] interval` of a profile's last change or raises within `raise_interval`, never
//! leaves a profile outside its bounds, and sizes each profile against the budget the changes before it in the same
//! look leave.

use std::collections::BTreeMap;

use crate::api::{Advice, ProfileSizing, Resize, SizingChange, cores};
use crate::config::{Admission, Config, Profile, cpu_budget};

use super::{BUSY, GRANULE, History, WAITED, Window, advise, busy, first_runner, spread, waits};

/// The fewest runs a raise of `slots` or `cpus` reads: the jobs queued since the profile's last change, or its runs
/// at its current `cpus`. A wrong raise costs a wait or a slower run, not a kill, so it does not wait for `min_jobs`.
const RAISE_RUNS: u64 = 3;

/// Runs whose CPU peak came within this share of their cap, in percent, at p95, were held back by it.
const AT_CAP: u64 = 95;

/// `cpus` are lowered once the largest CPU peak plus the margin is at most this share of them, in percent.
const FAR_UNDER: u64 = 50;

/// What auto sizing reads besides the config and the history.
#[derive(Debug, Clone, Copy)]
pub struct Context<'run> {
    /// The slice's `MemoryMax`; `None` when it is unlimited.
    pub memory_max: Option<u64>,
    /// The slice's CPUs, in millicores; `None` when they could not be read, and then `cpus` are never raised.
    pub cpus: Option<u64>,
    /// The history's window; `now` is when the changes are made.
    pub window: Window,
    /// When auto sizing last changed each profile, in seconds, by name, however long ago.
    pub last: &'run BTreeMap<String, u64>,
    /// How many runners each profile runs now, by name.
    pub running: &'run BTreeMap<String, u32>,
}

/// The changes to make now, in order, one per profile at most. Empty unless `[sizing] auto` is on.
///
/// A change that lowers `memory` or `cpus` waits `[sizing] interval` after the profile's last change; one that only
/// raises waits `raise_interval`. Each moves `memory` toward the advice, and `cpus` toward what its runs reach, by at
/// most `[sizing] max_step` percent, `memory` within the profile's `min_memory` and `max_memory`, `cpus` within the
/// slice's CPUs. A profile whose jobs waited for a slot, or whose slots were busy, since its last change gets one
/// more slot when every profile's slots still fit `MemoryMax`: at their caps under `caps` admission, at their jobs'
/// p95 peaks under `measured`; at most `max_slots` when set, and never under what runs. Under `caps` a lowered
/// `memory` adds the slots the advice gives. Each change is sized as if the ones before it were written: they take
/// budget the later ones cannot.
pub fn changes(config: &Config, history: &History, context: Context<'_>) -> Vec<SizingChange> {
    let rules = &config.sizing;
    if !rules.auto {
        return Vec::new();
    }
    let window = context.window;
    let sizing = advise(config, context.memory_max, history, window);
    let now = window.now / 1000;
    let mut budget = Budget::new(config, &sizing.profiles);
    let mut changes = Vec::new();
    for advised in &sizing.profiles {
        let Some(profile) = config.profiles.get(&advised.name) else {
            continue;
        };
        let last = context.last.get(&advised.name).copied();
        let since = last.map_or(u64::MAX, |at| now.saturating_sub(at));
        if since < rules.raise_after().as_secs() {
            continue;
        }
        let lowering = if since >= rules.interval.as_secs() {
            Lowering::Allowed
        } else {
            Lowering::Held
        };
        let fresh = Fresh::read(history, advised, last, window);
        let mut reasons = Vec::new();
        let memory = memory(
            config,
            profile,
            advised.advice,
            met_cap(history, &advised.name),
            lowering,
        );
        if memory != profile.memory.0 {
            reasons.push(sizing.memory_line(advised));
        }
        let running = context.running.get(&advised.name).copied().unwrap_or(0);
        let slots = match config.host.admission {
            Admission::Caps => {
                caps_slots(profile, advised, &fresh, memory, &context, &budget, running)
            }
            Admission::Measured => {
                measured_slots(config, &sizing.profiles, advised, &fresh, &context, &budget)
            }
        };
        // Under caps a lowered memory's slots are the advice's, which its line says.
        let advised_slots = config.host.admission == Admission::Caps && memory < profile.memory.0;
        if slots > profile.slots && !advised_slots {
            reasons.push(fresh.waited(last.is_some()));
        }
        let cpus = cpus(
            config, profile, advised, history, &context, &budget, slots, lowering,
        );
        if let Some(cpus) = &cpus {
            reasons.push(cpus.reason.clone());
        }
        let from = Resize {
            memory: profile.memory.0,
            slots: profile.slots,
            cpus: cpus.as_ref().map(|_| profile.cpus),
        };
        let to = Resize {
            memory,
            slots,
            cpus: cpus.map(|cpus| cpus.to),
        };
        if to == from || slots == 0 || !within(profile, from, to) {
            continue;
        }
        budget.caps = budget
            .caps
            .saturating_sub(from.memory.saturating_mul(u64::from(from.slots)))
            .saturating_add(to.memory.saturating_mul(u64::from(to.slots)));
        let millicores = |resize: Resize| {
            u64::from(resize.cpus.unwrap_or(profile.cpus)) * 1000 * u64::from(resize.slots)
        };
        budget.cpus = budget
            .cpus
            .saturating_sub(millicores(from))
            .saturating_add(millicores(to));
        budget.slots.insert(advised.name.clone(), to.slots);
        changes.push(SizingChange {
            at: now,
            profile: advised.name.clone(),
            from,
            to,
            reason: reasons.join("; "),
            uncertain: false,
        });
    }
    changes
}

/// Whether a change may lower `memory` or `cpus`: only `interval` after the profile's last change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lowering {
    Allowed,
    Held,
}

/// The budget as the changes picked so far leave it.
struct Budget {
    /// `slots × memory` over all profiles, with the mirror.
    caps: u64,
    /// `slots × cpus` over all profiles, in millicores.
    cpus: u64,
    /// Each profile's slots.
    slots: BTreeMap<String, u32>,
}

impl Budget {
    /// The budget the config takes as it is.
    fn new(config: &Config, profiles: &[ProfileSizing]) -> Budget {
        Budget {
            caps: config.memory_caps().map_or(u64::MAX, |caps| caps.0),
            cpus: config
                .profiles
                .values()
                .map(|profile| u64::from(profile.cpus) * 1000 * u64::from(profile.slots))
                .sum(),
            slots: profiles
                .iter()
                .map(|advised| (advised.name.clone(), advised.slots))
                .collect(),
        }
    }
}

/// What the jobs queued since a profile's last change say about its slots: a raise reads only them, so one slot
/// that fixed a month's waits is not followed by another every `raise_interval`.
struct Fresh {
    jobs: u64,
    /// Seconds from queued to started, p95.
    wait: u64,
    busy: u32,
}

impl Fresh {
    /// The profile's jobs queued since `last` (seconds), or in the whole window without a last change.
    fn read(
        history: &History,
        advised: &ProfileSizing,
        last: Option<u64>,
        window: Window,
    ) -> Fresh {
        let start = last.map_or(window.since, |at| at.saturating_mul(1000).max(window.since));
        let jobs: Vec<_> = history
            .jobs
            .iter()
            .filter(|job| job.profile == advised.name && job.queued_at >= start)
            .collect();
        Fresh {
            jobs: jobs.len() as u64,
            wait: waits(&jobs).p95,
            busy: busy(
                history,
                &advised.name,
                advised.slots,
                first_runner(history, start),
                window.now,
            ),
        }
    }

    /// Whether another slot would help: enough jobs, which waited a minute at p95 or kept the slots busy half the
    /// time.
    fn wants_slots(&self) -> bool {
        self.jobs >= RAISE_RUNS && (self.wait >= WAITED || self.busy >= BUSY)
    }

    /// Why a slot is added.
    fn waited(&self, changed: bool) -> String {
        format!(
            "{} jobs {}waited {}s for a slot (p95) and slots were busy {}%; one more fits the slice beside the \
             other profiles'",
            self.jobs,
            if changed {
                "since its last change "
            } else {
                ""
            },
            self.wait,
            self.busy
        )
    }
}

/// Whether `to` keeps the profile within its bounds: `memory` within `min_memory` and `max_memory`, and `slots` at
/// most `max_slots` when they grow.
fn within(profile: &Profile, from: Resize, to: Resize) -> bool {
    profile.min_memory.is_none_or(|min| to.memory >= min.0)
        && profile.max_memory.is_none_or(|max| to.memory <= max.0)
        && (to.slots <= from.slots || profile.max_slots.is_none_or(|max| to.slots <= max))
}

/// Whether a run of the profile in the history, cancelled ones too, was killed for memory or met its cap.
fn met_cap(history: &History, profile: &str) -> bool {
    history
        .jobs
        .iter()
        .chain(&history.cancelled)
        .filter(|job| job.profile == profile)
        .any(|job| job.oom_killed || job.memory_cap.is_some_and(|cap| job.met_cap(cap)))
}

/// The profile's next `memory`: the advice's, at most `max_step` percent away and within its bounds, in steps of
/// `GRANULE` toward the advice. A lowering waits while a run met the cap, and while `lowering` holds it.
fn memory(
    config: &Config,
    profile: &Profile,
    advice: Advice,
    met_cap: bool,
    lowering: Lowering,
) -> u64 {
    let current = profile.memory.0;
    let step = current.saturating_mul(u64::from(config.sizing.max_step)) / 100;
    match advice {
        Advice::Lower { memory, .. } if !met_cap && lowering == Lowering::Allowed => {
            let floor = current
                .saturating_sub(step)
                .div_ceil(GRANULE)
                .saturating_mul(GRANULE);
            let bound = profile.min_memory.map_or(0, |min| min.0);
            memory.max(floor).max(bound).min(current)
        }
        Advice::Raise { memory, .. } => {
            let ceiling = current.saturating_add(step) / GRANULE * GRANULE;
            let bound = profile.max_memory.map_or(u64::MAX, |max| max.0);
            memory.min(ceiling).min(bound).max(current)
        }
        _ => current,
    }
}

/// Under `caps`: as many slots as the advice gives for the new memory. A lowering whose jobs waited adds as many as
/// the memory the slots take now holds, up to `max_slots`; a raise keeps as many as the slice holds beside the other
/// profiles, and 0, no change, while more than that run. With the memory kept, one more slot when its jobs waited
/// since the last change and its cap fits `MemoryMax` beside every other cap, up to `max_slots`. Otherwise the same.
fn caps_slots(
    profile: &Profile,
    advised: &ProfileSizing,
    fresh: &Fresh,
    memory: u64,
    context: &Context<'_>,
    budget: &Budget,
    running: u32,
) -> u32 {
    let current = profile.slots;
    let max_slots = profile.max_slots.unwrap_or(u32::MAX);
    let fit = |room: u64| u32::try_from(room / memory.max(1)).unwrap_or(u32::MAX);
    if memory < profile.memory.0 && advised.wants_slots {
        let share = profile.memory.0.saturating_mul(u64::from(current));
        return fit(share).min(max_slots).max(current);
    }
    let Some(memory_max) = context.memory_max else {
        return current;
    };
    if memory > profile.memory.0 {
        let share = profile.memory.0.saturating_mul(u64::from(current));
        let room = memory_max.saturating_sub(budget.caps.saturating_sub(share));
        // Fewer slots than run would leave the running ones over the budget: wait for some to finish.
        let slots = fit(room).min(current);
        return if slots < running { 0 } else { slots };
    }
    if fresh.wants_slots()
        && current < max_slots
        && budget.caps.saturating_add(memory) <= memory_max
    {
        return current + 1;
    }
    current
}

/// Under `measured`: one more slot when its jobs waited since the last change and every profile's slots, this one's
/// with the new one, fit the slice beside the mirror at their jobs' p95 peaks (at its cap, a profile with fewer than
/// `min_jobs` jobs), up to `max_slots`, with the slots the changes before it gave. Without `max_slots` or a
/// `MemoryMax`, nothing bounds slots, so none is added. Otherwise the same.
fn measured_slots(
    config: &Config,
    profiles: &[ProfileSizing],
    advised: &ProfileSizing,
    fresh: &Fresh,
    context: &Context<'_>,
    budget: &Budget,
) -> u32 {
    let current = advised.slots;
    let max_slots = config
        .profiles
        .get(&advised.name)
        .and_then(|profile| profile.max_slots);
    if !fresh.wants_slots() || max_slots.is_some_and(|max| max <= current) {
        return current;
    }
    let Some(memory_max) = context.memory_max else {
        return if max_slots.is_some() {
            current + 1
        } else {
            current
        };
    };
    let peaks: u64 = profiles
        .iter()
        .map(|other| {
            let peak = if matches!(other.advice, Advice::TooFew { .. }) {
                other.memory
            } else {
                other.peaks.memory.p95
            };
            let slots = if other.name == advised.name {
                current + 1
            } else {
                budget
                    .slots
                    .get(&other.name)
                    .copied()
                    .unwrap_or(other.slots)
            };
            peak.saturating_mul(u64::from(slots))
        })
        .fold(config.mirror_memory(), u64::saturating_add);
    if peaks <= memory_max {
        current + 1
    } else {
        current
    }
}

/// A new `cpus` and why.
struct Cpus {
    to: u32,
    reason: String,
}

/// The profile's next `cpus`, from the CPU peaks of its runs whose container had them; `None` to keep them.
///
/// With `RAISE_RUNS` such runs peaking within `AT_CAP` percent of the cap at p95, `max_step` percent more, at least
/// 1, up to the slice's CPUs; under `caps` admission with a CPU budget, only as many as every profile's `slots ×
/// cpus` fit in it, at the `slots` this change gives. With `min_jobs` runs whose largest peak plus the margin is at
/// most `FAR_UNDER` percent of the cap, down to that, rounded up, by at most `max_step` percent, while `lowering`
/// allows it.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one thing a CPU change reads; a struct for them would be read here only"
)]
fn cpus(
    config: &Config,
    profile: &Profile,
    advised: &ProfileSizing,
    history: &History,
    context: &Context<'_>,
    budget: &Budget,
    slots: u32,
    lowering: Lowering,
) -> Option<Cpus> {
    let current = profile.cpus;
    let cap = u64::from(current) * 1000;
    let peaks: Vec<u64> = history
        .jobs
        .iter()
        .filter(|job| job.profile == advised.name && job.cpu_cap == Some(cap))
        .map(|job| job.cpu_peak)
        .collect();
    let runs = peaks.len() as u64;
    let spread = spread(peaks);
    let step = (current.saturating_mul(config.sizing.max_step) / 100).max(1);
    if runs >= RAISE_RUNS && spread.p95.saturating_mul(100) >= cap.saturating_mul(AT_CAP) {
        let slice = context.cpus?;
        let mut to = current
            .saturating_add(step)
            .min(u32::try_from(slice / 1000).unwrap_or(u32::MAX));
        if let (Admission::Caps, Some(overcommit)) =
            (config.host.admission, config.host.cpu_overcommit)
        {
            let others = budget
                .cpus
                .saturating_sub(cap.saturating_mul(u64::from(profile.slots)));
            let room = cpu_budget(slice, overcommit).saturating_sub(others);
            let fit = room / 1000 / u64::from(slots.max(1));
            to = to.min(u32::try_from(fit).unwrap_or(u32::MAX));
        }
        return (to > current).then(|| Cpus {
            to,
            reason: format!(
                "{runs} runs at its {current} CPUs peaked at {} CPUs (p95), at the cap",
                cores(spread.p95)
            ),
        });
    }
    if lowering == Lowering::Held || runs < u64::from(config.sizing.min_jobs) {
        return None;
    }
    let largest = spread.max;
    let need =
        largest.saturating_add(largest.saturating_mul(u64::from(config.sizing.margin)) / 100);
    if need.saturating_mul(100) > cap.saturating_mul(FAR_UNDER) {
        return None;
    }
    let to = u32::try_from(need.div_ceil(1000))
        .unwrap_or(u32::MAX)
        .max(current.saturating_sub(step))
        .max(1);
    (to < current).then(|| Cpus {
        to,
        reason: format!(
            "{runs} runs at its {current} CPUs peaked at {} CPUs at most; + {}% is {}",
            cores(largest),
            config.sizing.margin,
            cores(need)
        ),
    })
}
