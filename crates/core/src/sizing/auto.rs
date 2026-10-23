//! Auto sizing: which profiles' `memory` and `slots` the daemon changes now, from the advice and within the limits
//! `[sizing]` and each profile set. Pure: the daemon reads the history and when each profile last changed, writes
//! each change through the config check as `PUT /v1/config` does, and records it. Never lowers a profile a run met
//! the cap of, never sizes a profile from fewer than `[sizing] min_jobs` jobs, never leaves a profile outside its
//! bounds, and sizes each profile against the budget the changes before it in the same look leave.

use std::collections::BTreeMap;

use crate::api::{Advice, ProfileSizing, Resize, SizingChange};
use crate::config::{Admission, Config, Profile};

use super::{GRANULE, History, Window, advise};

/// What auto sizing reads besides the config and the history.
#[derive(Debug, Clone, Copy)]
pub struct Context<'run> {
    /// The slice's `MemoryMax`; `None` when it is unlimited.
    pub memory_max: Option<u64>,
    /// The history's window; `now` is when the changes are made.
    pub window: Window,
    /// When auto sizing last changed each profile, in seconds, by name, however long ago.
    pub last: &'run BTreeMap<String, u64>,
    /// How many runners each profile runs now, by name.
    pub running: &'run BTreeMap<String, u32>,
}

/// The changes to make now, in order: one per profile whose advice is to change it, that no change in the last
/// `[sizing] interval` touched. Empty unless `[sizing] auto` is on. Each moves `memory` toward the advice by at most
/// `[sizing] max_step` percent, within the profile's `min_memory` and `max_memory`. Under `caps` admission `slots`
/// follow the memory as the advice has them, at most `max_slots`, and never under what runs; under `measured`, a
/// profile whose jobs wait gets one more slot, up to `max_slots`, when every profile's slots fit the slice at their
/// jobs' p95 peaks. Each change is sized as if the ones before it were written: they take budget the later ones
/// cannot.
pub fn changes(config: &Config, history: &History, context: Context<'_>) -> Vec<SizingChange> {
    let rules = &config.sizing;
    if !rules.auto {
        return Vec::new();
    }
    let window = context.window;
    let sizing = advise(config, context.memory_max, history, window);
    let now = window.now / 1000;
    let interval = rules.interval.as_secs();
    let mut budget = Budget {
        caps: config.memory_caps().map_or(u64::MAX, |caps| caps.0),
        slots: sizing
            .profiles
            .iter()
            .map(|advised| (advised.name.clone(), advised.slots))
            .collect(),
    };
    let mut changes = Vec::new();
    for advised in &sizing.profiles {
        let Some(profile) = config.profiles.get(&advised.name) else {
            continue;
        };
        if context
            .last
            .get(&advised.name)
            .is_some_and(|at| now < at.saturating_add(interval))
        {
            continue;
        }
        let from = Resize {
            memory: profile.memory.0,
            slots: profile.slots,
        };
        let mut reasons = Vec::new();
        let memory = memory(
            config,
            profile,
            advised.advice,
            met_cap(history, &advised.name),
        );
        if memory != from.memory {
            reasons.push(sizing.memory_line(advised));
        }
        let running = context.running.get(&advised.name).copied().unwrap_or(0);
        let slots = match config.host.admission {
            Admission::Caps => caps_slots(profile, advised, memory, &context, &budget, running),
            Admission::Measured => {
                let slots = measured_slots(config, &sizing.profiles, advised, &context, &budget);
                if slots > from.slots {
                    reasons.push(format!(
                        "jobs waited {}s for a slot (p95) and slots were busy {}%; {slots} slots at their jobs' \
                         p95 peaks fit the slice beside the other profiles'",
                        advised.wait.p95, advised.busy
                    ));
                }
                slots
            }
        };
        let to = Resize { memory, slots };
        if to == from || slots == 0 || !within(profile, from, to) {
            continue;
        }
        budget.caps = budget
            .caps
            .saturating_sub(from.memory.saturating_mul(u64::from(from.slots)))
            .saturating_add(to.memory.saturating_mul(u64::from(to.slots)));
        budget.slots.insert(advised.name.clone(), to.slots);
        changes.push(SizingChange {
            at: now,
            profile: advised.name.clone(),
            from,
            to,
            reason: reasons.join("; "),
        });
    }
    changes
}

/// The budget as the changes picked so far leave it.
struct Budget {
    /// `slots × memory` over all profiles, with the mirror.
    caps: u64,
    /// Each profile's slots.
    slots: BTreeMap<String, u32>,
}

/// Whether `to` keeps the profile within its bounds: `memory` within `min_memory` and `max_memory`, and `slots` at
/// most `max_slots` when they grow.
fn within(profile: &Profile, from: Resize, to: Resize) -> bool {
    profile.min_memory.is_none_or(|min| to.memory >= min.0)
        && profile.max_memory.is_none_or(|max| to.memory <= max.0)
        && (to.slots <= from.slots || profile.max_slots.is_some_and(|max| to.slots <= max))
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
/// `GRANULE` toward the advice. A lowering waits while a run met the cap.
fn memory(config: &Config, profile: &Profile, advice: Advice, met_cap: bool) -> u64 {
    let current = profile.memory.0;
    let step = current.saturating_mul(u64::from(config.sizing.max_step)) / 100;
    match advice {
        Advice::Lower { memory, .. } if !met_cap => {
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
/// profiles, and 0, no change, while more than that run. Otherwise the same.
fn caps_slots(
    profile: &Profile,
    advised: &ProfileSizing,
    memory: u64,
    context: &Context<'_>,
    budget: &Budget,
    running: u32,
) -> u32 {
    let current = profile.slots;
    let fit = |room: u64| u32::try_from(room / memory.max(1)).unwrap_or(u32::MAX);
    if memory < profile.memory.0 && advised.wants_slots {
        let share = profile.memory.0.saturating_mul(u64::from(current));
        return fit(share)
            .min(profile.max_slots.unwrap_or(current))
            .max(current);
    }
    if memory > profile.memory.0 {
        let Some(memory_max) = context.memory_max else {
            return current;
        };
        let share = profile.memory.0.saturating_mul(u64::from(current));
        let room = memory_max.saturating_sub(budget.caps.saturating_sub(share));
        // Fewer slots than run would leave the running ones over the budget: wait for some to finish.
        let slots = fit(room).min(current);
        return if slots < running { 0 } else { slots };
    }
    current
}

/// Under `measured`: one more slot when its jobs waited and every profile's slots, this one's with the new one, fit
/// the slice beside the mirror at their jobs' p95 peaks (at its cap, a profile with fewer than `min_jobs` jobs), up
/// to `max_slots`, with the slots the changes before it gave. Otherwise the same.
fn measured_slots(
    config: &Config,
    profiles: &[ProfileSizing],
    advised: &ProfileSizing,
    context: &Context<'_>,
    budget: &Budget,
) -> u32 {
    let current = advised.slots;
    let wants = advised.wants_slots && !matches!(advised.advice, Advice::TooFew { .. });
    let Some(max_slots) = config
        .profiles
        .get(&advised.name)
        .and_then(|profile| profile.max_slots)
        .filter(|max| wants && *max > current)
    else {
        return current;
    };
    let Some(memory_max) = context.memory_max else {
        return (current + 1).min(max_slots);
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
