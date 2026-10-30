//! Auto sizing: which profiles' `memory`, `slots` and `cpus` the daemon changes now, from the advice, the runs since
//! each profile's last change and the limits `[sizing]` and each profile set. Pure: the daemon reads the history and
//! when each profile last changed, writes each change through the config check as `PUT /v1/config` does, and records
//! it. Never lowers a profile while a job is on its cap, never changes `memory` from fewer than `[sizing] min_jobs`
//! jobs, never lowers within `[sizing] interval` of a profile's last change or raises within `raise_interval`, never
//! leaves a profile outside its bounds, and sizes each profile against the budget the changes before it in the same
//! look leave. Reads `cpus` only from runs at the profile's current `cpus` since its last change and since the daemon
//! got them. Under `caps` admission raises them only on those runs' CPU averages, never on peaks: a parallel build
//! peaks at whatever cap it gets, and there `cpus` take budget. Under `measured`, where a running job reserves its
//! average, runs the cap held back raise them too, within what the CPU budget holds at caps, and nothing lowers them.
//! Under `caps` lowers them only when the cap held none of those runs back, never under their highest peak. A run
//! the cap held back is one its quota throttled in 15% of its run or more, or, without that figure, one whose bursts
//! met the cap (ADRs 0075 and 0079); under `measured` a raise goes on only while the one before made the job's runs
//! faster or less throttled.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::api::{
    Advice, ProfileSizing, Resize, Sizing, SizingChange, SizingRefused, ago, cores, duration,
    job_title,
};
use crate::config::{Admission, ByteSize, Config, Profile, cpu_budget};

use super::{
    BUSY, CpusSet, GRANULE, HELD, History, Throttling, WAITED, Window, advise, busy, first_runner,
    spread, waits,
};

/// The fewest runs a raise of `slots` or `cpus` reads: the jobs queued since the profile's last change, or one job's
/// runs at its current `cpus`. It does not wait for `min_jobs`: a slot too few costs every queued job a wait now.
pub(crate) const RAISE_RUNS: u64 = 3;

/// A job whose runs averaged this share of their CPU cap or more, in percent, at the median, is held back by it over
/// the whole run, not in a burst: its checkout, cache restore and upload steps barely use a CPU, so the rest of it
/// ran at the cap. On the owner's history no job's median came past 37%, whatever its cap: their builds are bursts.
const BOUND: u64 = 70;

/// `cpus` come down toward this many times the heaviest job's p95 CPU average, and never under the highest peak of
/// the runs read: a job back at the cap would average half of it, far under `BOUND`, so a lowering never asks for
/// the raise back.
const HEADROOM: u64 = 2;

/// A run without a throttling figure, as one from before schema 17, whose CPU peak came this close to its cap, in
/// percent, met it in a burst, and counts as held back (ADR 0075). Peaks are the highest 2-second interval, so one
/// can pass the cap a little.
const MET: u64 = 95;

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
    /// Each profile's `cpus` as the daemon ran them, oldest first: what a `cpus` change reads runs since, as a
    /// hand edit, a reload or an undo leaves no change of auto sizing's.
    pub cpus_set: &'run [CpusSet],
    /// How many runners each profile runs now, by name.
    pub running: &'run BTreeMap<String, u32>,
}

/// The changes to make now, in order, one per profile at most. Empty unless `[sizing] auto` is on.
///
/// A change that lowers `memory` or `cpus` waits `[sizing] interval` after the profile's last change; one that only
/// raises waits `raise_interval`. A lowering moves `memory` toward the advice, and `cpus` move toward what their runs
/// reach, by at most `[sizing] max_step` percent; a raise of `memory` goes straight to the advice, which only a run
/// that met the cap gives. `memory` stays within the profile's `min_memory` and `max_memory`, `cpus` within the
/// slice's CPUs. A lowering that went too far is taken back at once (`ProfileSizing::revert`). A profile whose jobs
/// waited for a slot, or whose slots were busy, since its last change gets one more slot when every profile's slots
/// still fit `MemoryMax`: at their caps under `caps` admission, at their jobs' p95 peaks under `measured`; at most
/// `max_slots` when set, and never under what runs. Under `caps` a lowered `memory` adds the slots the advice gives. Each change is sized as if the ones before it were written: they take
/// budget the later ones cannot.
///
/// `cpus` go up `max_step` percent, at least 1, when one job's runs at them since the profile's last change and since
/// the daemon got them (`Context::cpus_set`), 3 at least, averaged `BOUND` percent of them at the median, or under
/// `measured` admission were held back by them (throttled, or without that figure met in a burst) while the raise
/// before helped them (`unhelped`); up to the slice's CPUs, and with a CPU budget only as far as it holds: every
/// profile's `slots × cpus` under `caps`; under `measured`, what that admission reserves at caps, every profile's
/// warm runners and one more runner at the largest cap (`Budget::at_caps`). Under `caps` admission they come down, `interval` after the last change and by `max_step`
/// percent at most, toward `HEADROOM` times the heaviest job's p95 average or the highest peak of those runs,
/// whichever is larger, rounded up to whole CPUs, at least 1, once `min_jobs` such runs say so, every job among them
/// has a measured average, and the cap held none of them back. Under `measured` they never come down: they bound no
/// slot there. Only successful runs with a measured average count; see [`next_look`] for the raises it cannot make.
pub fn changes(config: &Config, history: &History, context: Context<'_>) -> Vec<SizingChange> {
    next_look(config, history, context).changes
}

/// What a look does: the changes [`changes`] picks, and the `cpus` raises it cannot make.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    /// The changes to make now, in order, one per profile at most.
    pub changes: Vec<SizingChange>,
    /// The profiles whose jobs the CPU cap holds back but whose `cpus` cannot go up: the slice's CPUs are unknown or
    /// all theirs, the CPU budget has no room, or under `measured` admission the raise before did not help them.
    pub refused: Vec<SizingRefused>,
}

/// The changes [`changes`] picks, and the `cpus` raises the same look cannot make.
pub fn next_look(config: &Config, history: &History, context: Context<'_>) -> Look {
    let rules = &config.sizing;
    if !rules.auto {
        return Look::default();
    }
    let window = context.window;
    let sizing = advise(config, context.memory_max, history, window);
    let now = window.now / 1000;
    let mut budget = Budget::new(config, &sizing.profiles);
    let mut changes = Vec::new();
    let mut refused = Vec::new();
    for advised in &sizing.profiles {
        let Some(profile) = config.profiles.get(&advised.name) else {
            continue;
        };
        let last = context.last.get(&advised.name).copied();
        let since = last.map_or(u64::MAX, |at| now.saturating_sub(at));
        if advised.revert.is_some() {
            match reverted(config, profile, advised, &sizing, &context, &budget) {
                Revert::Write(change) => {
                    budget.take(&advised.name, profile, change.from, change.to);
                    changes.push(change);
                    continue;
                }
                Revert::Wait => continue,
                // Raised as far as its bounds allow, as any raise is.
                Revert::Outside => {}
            }
        }
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
        let memory = memory(config, profile, advised.advice, lowering);
        let running = context.running.get(&advised.name).copied().unwrap_or(0);
        let slots = match config.host.admission {
            Admission::Caps => {
                caps_slots(profile, advised, &fresh, memory, &context, &budget, running)
            }
            Admission::Measured => {
                measured_slots(config, &sizing.profiles, advised, &fresh, &context, &budget)
            }
        };
        let (slots, cpus) = cpus_and_slots(
            config,
            profile,
            advised,
            history,
            fresh.start,
            &context,
            &budget,
            slots,
            lowering,
        );
        if memory != profile.memory.0 {
            reasons.push(memory_reason(
                &sizing,
                advised,
                memory,
                slots,
                config.sizing.max_step,
            ));
        }
        // Under caps a lowered memory's slots are its line's.
        let advised_slots = config.host.admission == Admission::Caps && memory < profile.memory.0;
        if slots > profile.slots && !advised_slots {
            reasons.push(fresh.waited(last.is_some()));
        }
        let cpus = cpus.taken(&advised.name, &mut reasons, &mut refused);
        let from = Resize {
            memory: profile.memory.0,
            slots: profile.slots,
            cpus: cpus.map(|_| profile.cpus),
        };
        let to = Resize {
            memory,
            slots,
            cpus,
        };
        if to == from || slots == 0 {
            continue;
        }
        budget.take(&advised.name, profile, from, to);
        changes.push(SizingChange {
            at: now,
            profile: advised.name.clone(),
            from,
            to,
            reason: reasons.join("; "),
            uncertain: false,
        });
    }
    Look { changes, refused }
}

/// How far back a config that lowers `memory` is compared with auto sizing's raises, in seconds: a day.
pub const UNDONE_RAISES: u64 = 24 * 3600;

/// What a new config undoes of auto sizing's `memory` raises: a line for each profile whose `memory` it lowers under
/// what one of `changes` raised it to in the `UNDONE_RAISES` before `now` (seconds), as a config saved from a text read
/// before the raise does. A raise comes only after the jobs needed more than the cap (ADR 0070), so they may run out
/// of memory again. The config is not refused: the owner may mean it.
pub fn undone_raises(
    running: &Config,
    new: &Config,
    changes: &[SizingChange],
    now: u64,
) -> Vec<String> {
    new.profiles
        .iter()
        .filter_map(|(name, profile)| {
            let memory = profile.memory.0;
            let before = running.profiles.get(name)?.memory.0;
            let raise = changes
                .iter()
                .filter(|change| {
                    change.profile == *name
                        && change.to.memory > change.from.memory
                        && now.saturating_sub(change.at) < UNDONE_RAISES
                })
                .max_by_key(|change| change.at)?;
            (memory < before && memory < raise.to.memory).then(|| {
                format!(
                    "{name}'s memory goes to {}, under the {} auto sizing raised it to {} because its jobs needed \
                     more than {}: they may run out of memory again",
                    ByteSize(memory),
                    ByteSize(raise.to.memory),
                    ago(raise.at, now),
                    ByteSize(raise.from.memory)
                )
            })
        })
        .collect()
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
    /// Each profile's `cpus`.
    each: BTreeMap<String, u32>,
}

impl Budget {
    /// Takes what a change of `profile` from `from` to `to` adds, and gives back what it frees.
    fn take(&mut self, name: &str, profile: &Profile, from: Resize, to: Resize) {
        self.caps = self
            .caps
            .saturating_sub(from.memory.saturating_mul(u64::from(from.slots)))
            .saturating_add(to.memory.saturating_mul(u64::from(to.slots)));
        let millicores = |resize: Resize| {
            u64::from(resize.cpus.unwrap_or(profile.cpus)) * 1000 * u64::from(resize.slots)
        };
        self.cpus = self
            .cpus
            .saturating_sub(millicores(from))
            .saturating_add(millicores(to));
        self.slots.insert(name.to_owned(), to.slots);
        self.each
            .insert(name.to_owned(), to.cpus.unwrap_or(profile.cpus));
    }

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
            each: config
                .profiles
                .iter()
                .map(|(name, profile)| (name.clone(), profile.cpus))
                .collect(),
        }
    }

    /// The CPUs, millicores, that measured admission holds at caps with `name` at `cpus`: every profile's warm
    /// runners idle at their caps (`warm_runners`), and one more runner at the largest cap, as one of a job with no
    /// prediction reserves.
    fn at_caps(&self, config: &Config, name: &str, cpus: u32) -> u64 {
        let each = |other: &str| {
            if other == name {
                cpus
            } else {
                self.each.get(other).copied().unwrap_or(0)
            }
        };
        let warm: u64 = config
            .profiles
            .keys()
            .map(|other| {
                let slots = self.slots.get(other).copied().unwrap_or(0);
                u64::from(warm_runners(config, other, slots)) * u64::from(each(other))
            })
            .sum();
        let largest = config
            .profiles
            .keys()
            .map(|other| each(other))
            .max()
            .unwrap_or(0);
        (warm + u64::from(largest)) * 1000
    }
}

/// How many warm runners `profile`'s pools keep at most, with `slots`: their `warm` added up, at most its slots.
fn warm_runners(config: &Config, profile: &str, slots: u32) -> u32 {
    config
        .repositories
        .iter()
        .filter(|repository| repository.profile == profile)
        .map(|repository| repository.warm)
        .sum::<u32>()
        .min(slots)
}

/// What the jobs queued since a profile's last change say about its slots: a raise reads only them, so one slot
/// that fixed a month's waits is not followed by another every `raise_interval`.
struct Fresh {
    /// When the evidence starts, in milliseconds: the last change, or the window's start.
    start: u64,
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
            start,
            jobs: jobs.len() as u64,
            wait: waits(&jobs, history.kept_since).p95,
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

/// Why `memory` changes, as the advice line says it of the `memory` and `slots` this change writes, not the
/// advice's own: a step of `max_step` stops short of a lowering's memory, and the slots that fit follow. A change
/// short of the advice says what it goes toward.
fn memory_reason(
    sizing: &Sizing,
    advised: &ProfileSizing,
    memory: u64,
    slots: u32,
    max_step: u32,
) -> String {
    let made = ProfileSizing {
        advice: match advised.advice {
            Advice::Lower { .. } => Advice::Lower { memory, slots },
            Advice::Raise { .. } => Advice::Raise { memory, slots },
            other => other,
        },
        ..advised.clone()
    };
    let line = sizing.memory_line(&made);
    match advised.advice.change() {
        Some(advice) if advice.memory != memory => format!(
            "{line}; a step toward {}, at most {max_step}% a change",
            ByteSize(advice.memory)
        ),
        _ => line,
    }
}

/// What a look does with a lowering that went too far.
enum Revert {
    /// Takes it back with this change.
    Write(SizingChange),
    /// Waits while more runners run than the slots it gives back hold, or while the changes before it in this look
    /// leave the slice no slot at its `memory`: nothing else changes the profile meanwhile.
    Wait,
    /// It gets no raise: the profile is sized as any other.
    Outside,
}

/// The change that takes back a lowering that went too far (`ProfileSizing::revert`): straight to the `memory`
/// it came from, or as far toward it as `[host] memory_ceiling` and `max_memory` allow, and, under caps, the slots it
/// had, without waiting for `raise_interval` or keeping to `max_step`, as an undo would.
fn reverted(
    config: &Config,
    profile: &Profile,
    advised: &ProfileSizing,
    sizing: &Sizing,
    context: &Context<'_>,
    budget: &Budget,
) -> Revert {
    let Advice::Raise { memory, slots } = advised.advice else {
        return Revert::Outside;
    };
    let from = Resize {
        memory: profile.memory.0,
        slots: profile.slots,
        cpus: None,
    };
    // The advice holds the slots the slice holds beside the config's other profiles; under caps, the changes before
    // it in this look may have taken some of that.
    let slots = match (config.host.admission, context.memory_max) {
        (Admission::Caps, Some(memory_max)) => {
            let share = profile.memory.0.saturating_mul(u64::from(profile.slots));
            let room = memory_max.saturating_sub(budget.caps.saturating_sub(share));
            slots.min(u32::try_from(room / memory.max(1)).unwrap_or(u32::MAX))
        }
        _ => slots,
    };
    if slots == 0 {
        return Revert::Wait;
    }
    let running = context.running.get(&advised.name).copied().unwrap_or(0);
    let to = Resize {
        memory,
        slots,
        cpus: None,
    };
    if config.host.admission == Admission::Caps && running > slots {
        return Revert::Wait;
    }
    Revert::Write(SizingChange {
        at: sizing.until,
        profile: advised.name.clone(),
        from,
        to,
        reason: sizing.memory_line(advised),
        uncertain: false,
    })
}

/// The profile's next `memory`: a lowering at most `max_step` percent away, in steps of `GRANULE`, and only while
/// `lowering` allows it; a raise straight to the advice's, since only a run that met the cap raises it (ADR 0070),
/// and a kill is direct evidence. The advice keeps to `min_memory` and `max_memory`, and lowers nothing while a job
/// is on its cap.
fn memory(config: &Config, profile: &Profile, advice: Advice, lowering: Lowering) -> u64 {
    let current = profile.memory.0;
    let step = current.saturating_mul(u64::from(config.sizing.max_step)) / 100;
    match advice {
        Advice::Lower { memory, .. } if lowering == Lowering::Allowed => {
            let floor = current
                .saturating_sub(step)
                .div_ceil(GRANULE)
                .saturating_mul(GRANULE);
            memory.max(floor).min(current)
        }
        Advice::Raise { memory, .. } => memory,
        _ => current,
    }
}

/// Under `caps`: as many slots as the advice gives for the new memory. A lowering whose jobs waited adds as many as
/// the memory the slots take now holds, up to the advice's slots, which keep to `max_slots`; a raise keeps as many as the slice holds beside the other
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
    let fit = |room: u64| u32::try_from(room / memory.max(1)).unwrap_or(u32::MAX);
    // A step toward the advice's memory fits no more slots than the advice gives, which keeps to `max_slots`.
    if let Advice::Lower { slots, .. } = advised.advice
        && memory < profile.memory.0
        && advised.wants_slots
    {
        let share = profile.memory.0.saturating_mul(u64::from(current));
        return fit(share).min(slots).max(current);
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
        && profile.max_slots.is_none_or(|max| current < max)
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

/// The profile's next `cpus` and `slots`, from the `slots` memory and waits chose: under `caps` with a CPU budget,
/// slots past what the budget holds at the new `cpus` are dropped, and the `cpus` sized again at the slots that stay,
/// so an extra slot the budget refuses does not also refuse a CPU raise that fits without it.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one thing a CPU change reads; a struct for them would be read here only"
)]
fn cpus_and_slots(
    config: &Config,
    profile: &Profile,
    advised: &ProfileSizing,
    history: &History,
    since: u64,
    context: &Context<'_>,
    budget: &Budget,
    slots: u32,
    lowering: Lowering,
) -> (u32, CpuStep) {
    let size = |slots: u32| {
        cpus(
            config, profile, advised, history, since, context, budget, slots, lowering,
        )
    };
    // Slots the CPU budget cannot hold would wait for CPU anyway, and their waits would ask for more.
    let fit = |slots: u32, cpus: &CpuStep| {
        let cpus = match cpus {
            CpuStep::To(cpus) => cpus.to,
            CpuStep::Keep | CpuStep::Refused(_) => profile.cpus,
        };
        if slots > profile.slots {
            cpu_slots(config, profile, context, budget, cpus)
                .map_or(slots, |fit| slots.min(fit.max(profile.slots)))
        } else {
            slots
        }
    };
    let mut cpus = size(slots);
    let fitted = fit(slots, &cpus);
    if fitted < slots {
        cpus = size(fitted);
    }
    (fit(fitted, &cpus), cpus)
}

/// Under `caps` admission with a CPU budget, the most slots of the profile at `cpus` each that fit the budget beside
/// every other profile's `slots × cpus`, as the changes before it left them; none more than it has when the slice's
/// CPUs are unknown. `None` when no CPU budget bounds slots.
fn cpu_slots(
    config: &Config,
    profile: &Profile,
    context: &Context<'_>,
    budget: &Budget,
    cpus: u32,
) -> Option<u32> {
    let (Admission::Caps, Some(overcommit)) = (config.host.admission, config.host.cpu_overcommit)
    else {
        return None;
    };
    let Some(slice) = context.cpus else {
        return Some(profile.slots);
    };
    let others = budget
        .cpus
        .saturating_sub(u64::from(profile.cpus) * 1000 * u64::from(profile.slots));
    let room = cpu_budget(slice, overcommit).saturating_sub(others);
    Some(u32::try_from(room / (u64::from(cpus.max(1)) * 1000)).unwrap_or(u32::MAX))
}

/// What a look does with a profile's `cpus`.
enum CpuStep {
    Keep,
    To(Cpus),
    /// Its jobs are held back by the cap, and this says why it cannot go up.
    Refused(String),
}

impl CpuStep {
    /// The new `cpus`, if any, its reason added to `reasons`, or a refused raise to `refused`.
    fn taken(
        self,
        profile: &str,
        reasons: &mut Vec<String>,
        refused: &mut Vec<SizingRefused>,
    ) -> Option<u32> {
        match self {
            CpuStep::To(cpus) => {
                reasons.push(cpus.reason);
                Some(cpus.to)
            }
            CpuStep::Refused(reason) => {
                refused.push(SizingRefused {
                    profile: profile.to_owned(),
                    reason,
                });
                None
            }
            CpuStep::Keep => None,
        }
    }
}

/// A new `cpus` and why.
struct Cpus {
    to: u32,
    reason: String,
}

/// A job: its repository, workflow and name.
type JobKey<'run> = (&'run str, Option<&'run str>, &'run str);

/// When the daemon started running the profile with `cpus`, in ms, by `set`: its first event since the last one
/// with other `cpus`. `now` when its latest has other `cpus`, as before a reload announces them; 0 with no events.
fn cpus_since(set: &[CpusSet], profile: &str, cpus: u32, now: u64) -> u64 {
    let mut since = None;
    let mut seen = false;
    for event in set.iter().filter(|event| event.profile == profile) {
        seen = true;
        if event.cpus != cpus {
            since = None;
        } else if since.is_none() {
            since = Some(event.at);
        }
    }
    since.unwrap_or(if seen { now } else { 0 })
}

/// What one job's runs a `cpus` change reads say.
#[derive(Default)]
struct JobCpu {
    /// Their measured averages, millicores.
    averages: Vec<u64>,
    /// The highest of their peaks, millicores.
    peak: u64,
    /// How many of them their quota throttled in `HELD` percent of their run or more.
    throttled: u64,
    /// How many of them without a throttling figure peaked within `MET` percent of the cap.
    peaked: u64,
    /// What their quota did to each of them.
    runs: Vec<Throttling>,
}

impl JobCpu {
    /// How many of them the cap held back: throttled where that was measured, else met in a burst.
    fn held(&self) -> u64 {
        self.throttled + self.peaked
    }

    /// Why `job`'s runs at `current` CPUs raise them under measured admission.
    fn held_reason(&self, job: JobKey<'_>, current: u32) -> String {
        let how = if self.peaked == 0 {
            format!("throttled in {HELD}% of their run or more")
        } else {
            format!(
                "throttled in {HELD}% of their run or more, or peaking at the cap where that was not measured"
            )
        };
        format!(
            "{} runs of {} were held back by its {current} CPUs, {how}: under measured admission they bound no slot, \
             so the cap only slows them",
            self.held(),
            job_named(job)
        )
    }
}

/// Whether the cap held a run back. With a throttling figure that alone says so, whatever the run's peak: a 2-second
/// peak at the cap says nothing of what the cap cost. Without one, as in runs from before schema 17 or on a kernel
/// that keeps none, its peak says so as it did before (ADR 0075): missing is not read as never throttled.
enum Hold {
    Throttled,
    Peaked,
    Free,
}

impl Hold {
    fn of(job: &super::Ran, cap: u64) -> Hold {
        match job.throttling().held() {
            Some(true) => Hold::Throttled,
            Some(false) => Hold::Free,
            None => Hold::peaked(job, cap),
        }
    }

    fn peaked(job: &super::Ran, cap: u64) -> Hold {
        if job.cpu_peak.saturating_mul(100) >= cap.saturating_mul(MET) {
            Hold::Peaked
        } else {
            Hold::Free
        }
    }
}

/// The runs of the profile a `cpus` change reads, by job: successful runs whose container had `cap`, queued at or
/// after `since` (ms), the later of the profile's last change and when it got these `cpus` (`cpus_since`). A run
/// without a measured average adds none; nor does an average of 0, which only a run sampled once records. Its job is
/// still listed, so a lowering can tell a job it knows nothing of from one it lacks.
fn runs_at<'run>(
    history: &'run History,
    profile: &str,
    cap: u64,
    since: u64,
) -> BTreeMap<JobKey<'run>, JobCpu> {
    let mut jobs: BTreeMap<JobKey<'run>, JobCpu> = BTreeMap::new();
    for job in &history.jobs {
        if job.profile == profile
            && job.cpu_cap == Some(cap)
            && job.succeeded
            && job.queued_at >= since
        {
            let read = jobs
                .entry((&job.repository, job.workflow.as_deref(), &job.name))
                .or_default();
            read.averages
                .extend(job.cpu_average.filter(|average| *average > 0));
            read.peak = read.peak.max(job.cpu_peak);
            match Hold::of(job, cap) {
                Hold::Throttled => read.throttled += 1,
                Hold::Peaked => read.peaked += 1,
                Hold::Free => {}
            }
            read.runs.push(job.throttling());
        }
    }
    jobs
}

/// One successful run of a job in a profile, as [`unhelped_raise`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuRun {
    /// Its container's CPU cap, millicores, when recorded.
    pub cap: Option<u64>,
    /// When it was queued, ms.
    pub queued_at: u64,
    /// What its quota did to it.
    pub throttling: Throttling,
}

/// A figure at the median of a job's runs at its current `cpus` and at the `cpus` before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Medians {
    /// At its current `cpus`.
    pub now: u64,
    /// At the `cpus` before.
    pub before: u64,
}

/// A raise of `cpus` that did not help a job: its runs neither took less time nor were throttled in less of their run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unhelped {
    /// The CPU cap before, millicores.
    pub from: u64,
    /// How long its runs took, ms, where that is known on both sides.
    pub took: Option<Medians>,
    /// The share of their run their quota throttled them in, per mille, where that is known on both sides.
    pub throttled: Option<Medians>,
}

/// Whether the raise that brought a job to `cap` (millicores) did not help it, by `runs`, its successful runs in one
/// profile at any cap, and `now`, those at `cap` the reader counts; `None` when it did, or when nothing says: no
/// raise came before, as when its latest runs at another cap had more, or fewer than `RAISE_RUNS` runs at the cap
/// before. It helped when its runs at `cap` took less time at the median than at the cap before, or were throttled
/// in less of their run. A tool that sizes its pool from the quota (the JVM's `availableProcessors`, cargo's `-j`,
/// Go's `GOMAXPROCS`) starts more workers at each raise and stays throttled, running no faster: its raises stop
/// there, as more `cpus` would only take memory and weight (ADR 0079).
pub fn unhelped_raise(runs: &[CpuRun], cap: u64, now: &[Throttling]) -> Option<Unhelped> {
    let from = runs
        .iter()
        .filter(|run| run.cap.is_some_and(|other| other != cap))
        .max_by_key(|run| run.queued_at)?
        .cap
        .filter(|before| *before < cap)?;
    let at_before: Vec<Throttling> = runs
        .iter()
        .filter(|run| run.cap == Some(from))
        .map(|run| run.throttling)
        .collect();
    if (at_before.len() as u64) < RAISE_RUNS {
        return None;
    }
    let median = |runs: &[Throttling], figure: fn(Throttling) -> Option<u64>| {
        let values: Vec<u64> = runs.iter().copied().filter_map(figure).collect();
        (!values.is_empty()).then(|| spread(values).p50)
    };
    let medians = |figure: fn(Throttling) -> Option<u64>| {
        Some(Medians {
            now: median(now, figure)?,
            before: median(&at_before, figure)?,
        })
    };
    let took = medians(Throttling::took);
    let throttled = medians(Throttling::share);
    let helped = |medians: Option<Medians>| medians.map(|medians| medians.now < medians.before);
    match (helped(took), helped(throttled)) {
        (None, None) | (Some(true), _) | (_, Some(true)) => None,
        _ => Some(Unhelped {
            from,
            took,
            throttled,
        }),
    }
}

/// Why the raise before did not help `job`, whose runs at `cap` are `now`, as [`unhelped_raise`] finds it.
fn unhelped(
    history: &History,
    profile: &str,
    job: JobKey<'_>,
    cap: u64,
    now: &JobCpu,
) -> Option<String> {
    let runs: Vec<CpuRun> = history
        .jobs
        .iter()
        .filter(|run| {
            run.profile == profile
                && run.succeeded
                && (
                    run.repository.as_str(),
                    run.workflow.as_deref(),
                    run.name.as_str(),
                ) == job
        })
        .map(|run| CpuRun {
            cap: run.cpu_cap,
            queued_at: run.queued_at,
            throttling: run.throttling(),
        })
        .collect();
    let unhelped = unhelped_raise(&runs, cap, &now.runs)?;
    let mut why = format!(
        "the raise from {} to its {} CPUs did not help it:",
        unhelped.from / 1000,
        cap / 1000
    );
    if let Some(took) = unhelped.took {
        let _ = write!(
            why,
            " its runs took {} at the median, against {} before",
            duration(took.now / 1000),
            duration(took.before / 1000)
        );
    }
    if let Some(throttled) = unhelped.throttled {
        let joined = if unhelped.took.is_some() { "," } else { "" };
        let _ = write!(
            why,
            "{joined} its quota throttled them in {}% of their run, against {}%",
            throttled.now / 10,
            throttled.before / 10
        );
    }
    Some(why)
}

/// Under measured admission, why the runs the cap held back raise `cpus` at `cap`: the job with the most held runs,
/// `RAISE_RUNS` at least, among those the raise before helped (`Ok`); or, when it helped none of them, why not, for
/// the one with the most (`Err`). `None` when no job's runs were held back that often. A job the raise before did not
/// help says nothing for another raise, but another held job still may.
fn held_raise(
    history: &History,
    profile: &str,
    jobs: &BTreeMap<JobKey<'_>, JobCpu>,
    cap: u64,
    current: u32,
) -> Option<Result<String, String>> {
    let (helped, refused): (Vec<_>, Vec<_>) = jobs
        .iter()
        .filter(|(_, read)| read.held() >= RAISE_RUNS)
        .map(|(job, read)| (*job, read, unhelped(history, profile, *job, cap, read)))
        .partition(|(_, _, why)| why.is_none());
    if let Some((job, read, _)) = helped.into_iter().max_by_key(|(_, read, _)| read.held()) {
        return Some(Ok(read.held_reason(job, current)));
    }
    let (job, read, why) = refused.into_iter().max_by_key(|(_, read, _)| read.held())?;
    Some(Err(format!(
        "{}; {}",
        read.held_reason(job, current),
        why.unwrap_or_default()
    )))
}

/// `owner/name workflow / job`, as a CPU change's reason names a job.
fn job_named(job: JobKey<'_>) -> String {
    let (repository, workflow, name) = job;
    format!("{repository} {}", job_title(workflow, name, None))
}

/// A raise of `name`'s `cpus` held back by the cap, as `held` says: `step` more, up to the slice's CPUs, and with a
/// CPU budget only as many as it holds: under `caps` every profile's `slots × cpus`, under `measured` every profile's
/// warm runners and one more runner at the largest cap (`Budget::at_caps`). Refused, with why, when that is none.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one thing a CPU raise reads; a struct for them would be read here only"
)]
fn raised(
    config: &Config,
    name: &str,
    profile: &Profile,
    context: &Context<'_>,
    budget: &Budget,
    slots: u32,
    step: u32,
    held: String,
) -> CpuStep {
    let current = profile.cpus;
    let cap = u64::from(current) * 1000;
    let Some(slice) = context.cpus else {
        return CpuStep::Refused(format!("{held}; the slice's CPUs could not be read"));
    };
    let slice_cpus = u32::try_from(slice / 1000).unwrap_or(u32::MAX);
    let mut to = current.saturating_add(step).min(slice_cpus);
    let mut why = format!("it has all {slice_cpus} of the slice's CPUs");
    if let (Admission::Caps, Some(overcommit)) = (config.host.admission, config.host.cpu_overcommit)
    {
        let others = budget
            .cpus
            .saturating_sub(cap.saturating_mul(u64::from(profile.slots)));
        let room = cpu_budget(slice, overcommit).saturating_sub(others);
        let fit = room / 1000 / u64::from(slots.max(1));
        let fit = u32::try_from(fit).unwrap_or(u32::MAX);
        if fit < to {
            to = fit;
            why = format!(
                "{} CPUs for each of its {slots} slots do not fit the CPU budget beside the other profiles'",
                current.saturating_add(1)
            );
        }
    }
    // Under measured admission a running job reserves its average, whatever its cap, but an idle warm runner and a
    // runner whose job has no prediction reserve the cap.
    if let (Admission::Measured, Some(overcommit)) =
        (config.host.admission, config.host.cpu_overcommit)
    {
        let room = cpu_budget(slice, overcommit);
        while to > current && budget.at_caps(config, name, to) > room {
            to -= 1;
            why = format!(
                "{} CPUs do not fit the CPU budget with every profile's warm runners and one more runner at the \
                 largest cap",
                current.saturating_add(1)
            );
        }
    }
    if to > current {
        CpuStep::To(Cpus { to, reason: held })
    } else {
        CpuStep::Refused(format!("{held}; {why}"))
    }
}

/// How many `cpus` one change moves a profile at `current` by: `[sizing] max_step` percent of them, at least 1.
pub(crate) fn cpu_step(config: &Config, current: u32) -> u32 {
    (current.saturating_mul(config.sizing.max_step) / 100).max(1)
}

/// The profile's next `cpus`, from the CPU averages of its runs at its current `cpus` since `since` (ms), the last
/// change, and since it got them (`runs_at`). A raise, `max_step` percent, at least 1, when one job's `RAISE_RUNS`
/// runs or more averaged `BOUND` percent of the cap at the median, or under `measured` admission were held back by it
/// (kept, not refused, when there is no room; refused when the raise before did not help them): up to the slice's
/// CPUs, and with a CPU budget only as many as `raised` fits in it, at the `slots` this change gives; refused, with
/// why, when that is none. A lowering, under `caps`
/// admission while `lowering` allows it, with `min_jobs` runs, none of a job without a measured average and none the
/// cap held back: toward `HEADROOM` times the heaviest job's p95 average or the highest peak, whichever is larger,
/// rounded up to whole CPUs, at least 1, by at most `max_step` percent.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one thing a CPU change reads; a struct for them would be read here only"
)]
fn cpus(
    config: &Config,
    profile: &Profile,
    advised: &ProfileSizing,
    history: &History,
    since: u64,
    context: &Context<'_>,
    budget: &Budget,
    slots: u32,
    lowering: Lowering,
) -> CpuStep {
    let current = profile.cpus;
    let cap = u64::from(current) * 1000;
    let since = since.max(cpus_since(
        context.cpus_set,
        &advised.name,
        current,
        context.window.now,
    ));
    let jobs = runs_at(history, &advised.name, cap, since);
    let step = cpu_step(config, current);
    let raise = |held| {
        raised(
            config,
            &advised.name,
            profile,
            context,
            budget,
            slots,
            step,
            held,
        )
    };
    let bound = jobs
        .iter()
        .filter(|(_, read)| read.averages.len() as u64 >= RAISE_RUNS)
        .map(|(job, read)| (*job, read.averages.len(), spread(read.averages.clone()).p50))
        .filter(|(_, _, median)| median.saturating_mul(100) >= cap.saturating_mul(BOUND))
        .max_by_key(|(_, _, median)| *median);
    if let Some((job, runs, median)) = bound {
        let held = format!(
            "{} runs of {} averaged {} of its {current} CPUs (median): the cap holds them back",
            runs,
            job_named(job),
            cores(median)
        );
        return raise(held);
    }
    // Under measured admission `cpus` bound no slot: a cap that holds runs back only slows them, and frees nothing.
    if config.host.admission == Admission::Measured {
        match held_raise(history, &advised.name, &jobs, cap, current) {
            // A burst the slice's CPUs hold back is the slice's to give, not the profile's.
            Some(Ok(held)) => {
                return match raise(held) {
                    CpuStep::Refused(_) => CpuStep::Keep,
                    step => step,
                };
            }
            Some(Err(why)) => return CpuStep::Refused(why),
            None => {}
        }
    }
    let runs: usize = jobs.values().map(|read| read.averages.len()).sum();
    let peak = jobs.values().map(|read| read.peak).max().unwrap_or(0);
    // Under measured admission `cpus` bound no slot, so a lowering frees nothing and only slows the bursts. A job
    // whose runs at the cap have no measured average may be the one that needs it, and one the cap held back ran as
    // fast as the cap let it: its average says what the cap allowed, not what it needs.
    if lowering == Lowering::Held
        || config.host.admission == Admission::Measured
        || (runs as u64) < u64::from(config.sizing.min_jobs)
        || jobs
            .values()
            .any(|read| read.averages.is_empty() || read.held() > 0)
    {
        return CpuStep::Keep;
    }
    let Some((job, heaviest)) = jobs
        .iter()
        .map(|(job, read)| (*job, spread(read.averages.clone()).p95))
        .max_by_key(|(_, p95)| *p95)
    else {
        return CpuStep::Keep;
    };
    let need = heaviest.saturating_mul(HEADROOM).max(peak);
    let target = u32::try_from(need.div_ceil(1000))
        .unwrap_or(u32::MAX)
        .max(1);
    if target >= current {
        return CpuStep::Keep;
    }
    let to = target.max(current.saturating_sub(step)).max(1);
    let mut reason = format!(
        "{runs} runs at its {current} CPUs peaked at {} CPUs at most, none held back by the cap, and the heaviest job, {}, \
         averaged {} CPUs (p95); the larger of that peak and twice that average is {target}",
        cores(peak),
        job_named(job),
        cores(heaviest)
    );
    if to != target {
        let _ = write!(
            reason,
            "; a step toward {target}, at most {}% a change",
            config.sizing.max_step
        );
    }
    CpuStep::To(Cpus { to, reason })
}
