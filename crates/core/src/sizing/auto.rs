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
    Advice, ProfileSizing, Resize, SLOT_DISK, Sizing, SizingChange, SizingRefused, ago, cores,
    duration, job_title,
};
use crate::cache::docker_budget_gib;
use crate::config::{Admission, ByteSize, Config, Profile, cpu_budget};

use super::{
    CpusSet, GRANULE, HELD, History, SlotsSet, Throttling, Window, advise, busy, first_runner,
    peak, short_of_slots, slot_asking_wait, spread, wants_slots,
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

/// How long a profile's slots must stay `IDLE_SLOTS` above what ran at once before auto sizing takes one away, in ms:
/// a week, so a quiet weekend does not take back what the weekdays' bursts need.
pub const IDLE_FOR: u64 = 7 * 24 * 3600 * 1000;

/// A profile keeps this many slots more than ran at once, warm runners counted, before one is taken away: with one, a
/// burst one slot past the week's peak would add back the slot just taken.
const IDLE_SLOTS: u32 = 2;

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
    /// What the slots' Docker Engines may keep together, in bytes, from the last measure (`cache::docker_disk`);
    /// `None` before the first, and then no slot is added (ADR 0090).
    pub disk: Option<u64>,
}

/// The changes to make now, in order, one per profile at most. Empty unless `[sizing] auto` is on.
///
/// A change that lowers `memory` or `cpus` waits `[sizing] interval` after the profile's last change; one that only
/// raises waits `raise_interval`. A lowering moves `memory` toward the advice, and `cpus` move toward what their runs
/// reach, by at most `[sizing] max_step` percent; a raise of `memory` goes straight to the advice, which only a run
/// that met the cap gives. `memory` stays within the profile's `min_memory` and `max_memory`, `cpus` within the
/// slice's CPUs. A lowering that went too far is taken back at once (`ProfileSizing::revert`). Under `caps`
/// admission a profile whose jobs waited for a slot, or whose slots were busy, since its last change gets one more
/// slot when every profile's caps still fit `MemoryMax`, and a lowered `memory` adds the slots the advice gives. Under
/// `measured` a profile whose jobs waited for a slot while memory and CPU had room for them (`Ran::slot_waited`)
/// since its last change gets as many more as waited at once, at most `max_step` percent of its slots, at least 1,
/// while every slot keeps `SLOT_DISK` of Docker's disk (`measured_slots`), or a refusal says why it cannot; and one
/// whose slots stayed `IDLE_SLOTS` above what ran at once for `IDLE_FOR` since they were last set loses one. Never past `max_slots`, never under what
/// runs. Each change is sized as if the ones before it were written: they take budget the later ones cannot.
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

/// What a look does: the changes [`changes`] picks, and the `cpus` and `slots` raises it cannot make.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    /// The changes to make now, in order, one per profile at most.
    pub changes: Vec<SizingChange>,
    /// The profiles whose jobs the CPU cap holds back but whose `cpus` cannot go up: the slice's CPUs are unknown or
    /// all theirs, the CPU budget has no room, or under `measured` admission the raise before did not help them.
    pub refused: Vec<SizingRefused>,
    /// Under `measured` admission, the profiles whose jobs waited for a slot alone but whose `slots` cannot go up:
    /// Docker's disk is not measured yet, they are at `max_slots`, or one more slot would leave every slot less than
    /// `SLOT_DISK` of Docker's disk.
    pub refused_slots: Vec<SizingRefused>,
    /// Under `measured` admission, the profiles changed before whose jobs waited for a slot alone in the window but
    /// whose jobs since the change do not ask for one (`Fresh::wants_slots`): no slot is added for the waits before.
    pub waited_before_change: Vec<String>,
}

/// The changes [`changes`] picks, and the `cpus` and `slots` raises the same look cannot make.
pub fn next_look(config: &Config, history: &History, context: Context<'_>) -> Look {
    let rules = &config.sizing;
    if !rules.auto {
        return Look::default();
    }
    let window = context.window;
    let sizing = advise(config, context.memory_max, history, window);
    let now = window.now / 1000;
    let mut budget = Budget::new(config, &sizing.profiles);
    let mut look = Look::default();
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
                    look.changes.push(change);
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
        let lowering = Lowering::after(since, rules.interval.as_secs());
        let fresh = Fresh::read(history, advised, config.host.admission, last, window);
        let mut reasons = Vec::new();
        let memory = memory(config, profile, advised.advice, lowering);
        let running = context.running.get(&advised.name).copied().unwrap_or(0);
        let (slots, slots_reason) = match config.host.admission {
            Admission::Caps => (
                caps_slots(profile, advised, &fresh, memory, &context, &budget, running),
                None,
            ),
            Admission::Measured => {
                measured_slots(config, profile, advised, &fresh, &context, &budget)
                    .step(&advised.name, &mut look.refused_slots, || {
                        (lowering == Lowering::Allowed)
                            .then(|| idle_slots(config, history, advised, &context, running))?
                    })
                    .map_or((advised.slots, None), |step| (step.to, Some(step.reason)))
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
        if let Some(reason) = slots_reason {
            reasons.push(reason);
        } else if slots > profile.slots && !advised_slots {
            reasons.push(fresh.waited(last.is_some()));
        }
        let cpus = cpus.taken(&advised.name, &mut reasons, &mut look.refused);
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
        look.changes.push(SizingChange {
            at: now,
            profile: advised.name.clone(),
            from,
            to,
            reason: reasons.join("; "),
            uncertain: false,
        });
    }
    look.waited_before_change = waited_before_change(config, history, &sizing, &context);
    look
}

/// Under `measured` admission, the profiles changed before whose jobs in the window waited for a slot alone
/// (`ProfileSizing::wants_slots`) but whose jobs since the change do not ask for one (`Fresh::wants_slots`), which is
/// all a look reads: the advice must not promise a slot for the waits before the change.
fn waited_before_change(
    config: &Config,
    history: &History,
    sizing: &Sizing,
    context: &Context<'_>,
) -> Vec<String> {
    if config.host.admission != Admission::Measured {
        return Vec::new();
    }
    sizing
        .profiles
        .iter()
        .filter(|advised| advised.wants_slots)
        .filter(|advised| {
            let Some(last) = context.last.get(&advised.name).copied() else {
                return false;
            };
            !Fresh::read(
                history,
                advised,
                config.host.admission,
                Some(last),
                context.window,
            )
            .wants_slots()
        })
        .map(|advised| advised.name.clone())
        .collect()
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

impl Lowering {
    /// Whether a change `since` seconds after the profile's last one may lower it, with `interval` seconds between.
    fn after(since: u64, interval: u64) -> Lowering {
        if since >= interval {
            Lowering::Allowed
        } else {
            Lowering::Held
        }
    }
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
    /// Whether they ask for more slots (`sizing::wants_slots`), however few.
    wants: bool,
    /// Seconds they waited that ask for a slot under `admission` (`slot_asking_wait`), p95.
    wait: u64,
    /// The most of them that waited only for a slot at once.
    short: u32,
    busy: u32,
}

impl Fresh {
    /// The profile's jobs queued since `last` (seconds), or in the whole window without a last change.
    fn read(
        history: &History,
        advised: &ProfileSizing,
        admission: Admission,
        last: Option<u64>,
        window: Window,
    ) -> Fresh {
        let start = last.map_or(window.since, |at| at.saturating_mul(1000).max(window.since));
        let jobs: Vec<_> = history
            .jobs
            .iter()
            .filter(|job| job.profile == advised.name && job.queued_at >= start)
            .collect();
        let busy = busy(
            history,
            &advised.name,
            advised.slots,
            first_runner(history, start),
            window.now,
        );
        Fresh {
            start,
            jobs: jobs.len() as u64,
            wants: wants_slots(admission, &jobs, history.kept_since, busy),
            wait: slot_asking_wait(admission, &jobs, history.kept_since),
            short: short_of_slots(&jobs),
            busy,
        }
    }

    /// Whether another slot would help: enough jobs, which waited a minute for a slot at p95, or under `caps` kept
    /// the slots busy half the time.
    fn wants_slots(&self) -> bool {
        self.jobs >= RAISE_RUNS && self.wants
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

/// A change of a profile's slots under `measured` admission, and why.
struct SlotStep {
    to: u32,
    reason: String,
}

/// What a profile's jobs since its last change ask of its slots under `measured` admission.
enum Raise {
    /// No more: nothing waited for a slot alone.
    Unasked,
    /// This many, and why.
    To(SlotStep),
    /// More, which cannot be added, and why.
    Refused(String),
}

impl Raise {
    /// The change of `profile`'s slots to make: the raise, or with none asked, `idle`'s lowering. A refused raise
    /// goes to `refused`, and its jobs, which waited for a slot since the last change, keep the slots they have.
    fn step(
        self,
        profile: &str,
        refused: &mut Vec<SizingRefused>,
        idle: impl FnOnce() -> Option<SlotStep>,
    ) -> Option<SlotStep> {
        match self {
            Raise::To(step) => Some(step),
            Raise::Refused(reason) => {
                refused.push(SizingRefused::new(profile, reason));
                None
            }
            Raise::Unasked => idle(),
        }
    }
}

/// Under `measured`, more slots when its jobs since the last change waited for one while memory and CPU had room
/// (`Fresh::wants_slots`): as many as waited at once, at least 1 and at most `max_step` percent of its slots, up to
/// `max_slots`, and only as many as leave every slot, with the slots the changes before it gave, `SLOT_DISK` of
/// Docker's disk. A slot under measured admission costs no memory or CPU: each runner reserves its job's prediction
/// when it starts, and the budgets stop a start that does not fit. What it costs is disk: every slot's Docker store
/// shares the same quarter of it (`cache::docker_budget_gib`). Refused when Docker's disk was not measured yet, the
/// profile is at `max_slots`, or one more slot leaves too little disk.
fn measured_slots(
    config: &Config,
    profile: &Profile,
    advised: &ProfileSizing,
    fresh: &Fresh,
    context: &Context<'_>,
    budget: &Budget,
) -> Raise {
    let current = advised.slots;
    if !fresh.wants_slots() {
        return Raise::Unasked;
    }
    let asked = format!(
        "{} jobs {}waited {}s for a slot (p95) while memory and CPU had room, {} at once at most",
        fresh.jobs,
        if context.last.contains_key(&advised.name) {
            "since its last change "
        } else {
            ""
        },
        fresh.wait,
        fresh.short,
    );
    let max = profile.max_slots.unwrap_or(u32::MAX);
    if current >= max {
        return Raise::Refused(format!("{asked}; it is at its max_slots, {max}"));
    }
    let Some(disk) = context.disk else {
        return Raise::Refused(format!(
            "{asked}; Docker's disk is not measured yet, which the next check of the caches does"
        ));
    };
    let step = (current.saturating_mul(config.sizing.max_step) / 100).max(1);
    let wanted = current.saturating_add(fresh.short.clamp(1, step)).min(max);
    let others: u32 = budget
        .slots
        .iter()
        .filter(|(name, _)| **name != advised.name)
        .map(|(_, slots)| *slots)
        .sum();
    let Some(to) = (current.saturating_add(1)..=wanted)
        .rev()
        .find(|slots| share(disk, others.saturating_add(*slots)) >= SLOT_DISK)
    else {
        let next = current.saturating_add(1);
        return Raise::Refused(format!(
            "{asked}; at {next} slots, beside the other profiles' {others}, each slot would keep {}, under the {} \
             auto sizing leaves each slot",
            kept(
                disk,
                others.saturating_add(current),
                others.saturating_add(next)
            ),
            ByteSize(SLOT_DISK),
        ));
    };
    Raise::To(SlotStep {
        to,
        reason: format!(
            "{asked}; at {to} slots each slot keeps {}",
            kept(
                disk,
                others.saturating_add(current),
                others.saturating_add(to)
            )
        ),
    })
}

/// One slot's share of the `disk` the slots' Docker stores may keep together among `slots`, in bytes, as the
/// runners get it.
fn share(disk: u64, slots: u32) -> u64 {
    docker_budget_gib(disk, u64::from(slots)) << 30
}

/// What a slot keeps of Docker's disk at `to` slots in all, and what it keeps at `from` when that is more: `2G of
/// Docker's disk, down from 3G`.
fn kept(disk: u64, from: u32, to: u32) -> String {
    let (before, after) = (share(disk, from), share(disk, to));
    if after < before {
        format!(
            "{} of Docker's disk, down from {}",
            ByteSize(after),
            ByteSize(before)
        )
    } else {
        format!("{} of Docker's disk", ByteSize(after))
    }
}

/// Under `measured`, one slot fewer when, for the `IDLE_FOR` before now, its slots stayed `IDLE_SLOTS` or more above
/// the most runners that held one for a job at once, its warm runners counted: an idle slot shrinks every slot's
/// share of Docker's disk and keeps a volume. Only once the history holds a runner from before that time and the
/// profile has had its slots that long, by its `profile.changed` events and auto sizing's last change of its slots,
/// and `min_jobs` of its jobs ran in it, so a host that was off says nothing; never under what runs or 1. This takes
/// back a raise made by hand as well (ADR 0090).
fn idle_slots(
    config: &Config,
    history: &History,
    advised: &ProfileSizing,
    context: &Context<'_>,
    running: u32,
) -> Option<SlotStep> {
    let current = advised.slots;
    let now = context.window.now;
    let start = now.checked_sub(IDLE_FOR)?;
    let set = slots_since(&history.slots_set, &advised.name, current, now);
    // A change of its memory or cpus leaves the slots as they ran.
    let changed = history
        .changes
        .iter()
        .filter(|change| change.profile == advised.name && change.to.slots != change.from.slots)
        .map(|change| change.at.saturating_mul(1000))
        .max()
        .unwrap_or(0);
    // The history must cover the whole time: a runner from before it, the profile's slots and no change of them since.
    let first = history
        .runners
        .iter()
        .map(|runner| runner.started_at)
        .min()?;
    if set.max(changed).max(first) > start {
        return None;
    }
    let jobs = history
        .jobs
        .iter()
        .filter(|job| job.profile == advised.name && job.queued_at >= start)
        .count() as u64;
    if jobs < u64::from(config.sizing.min_jobs) {
        return None;
    }
    let mut edges: Vec<(u64, i32)> = history
        .runners
        .iter()
        .filter(|runner| {
            runner.profile == advised.name && runner.removed_at.is_none_or(|at| at > start)
        })
        .flat_map(|runner| {
            [
                (runner.started_at.max(start), 1),
                (runner.removed_at.unwrap_or(now).min(now), -1),
            ]
        })
        .collect();
    let ran = peak(&mut edges);
    let warm = warm_runners(config, &advised.name, current);
    if ran.saturating_add(warm).saturating_add(IDLE_SLOTS) > current {
        return None;
    }
    let to = current.saturating_sub(1).max(running).max(1);
    (to < current).then(|| SlotStep {
        to,
        reason: format!(
            "at most {ran} of its {current} slots held a runner for a job at once in the last {}{}: an idle slot \
             shrinks every slot's share of Docker's disk",
            duration(IDLE_FOR / 1000),
            if warm > 0 {
                format!(", beside {warm} warm runners")
            } else {
                String::new()
            }
        ),
    })
}

/// When the daemon started running the profile with `slots`, in ms, by `set`, as `cpus_since` reads `cpus`.
fn slots_since(set: &[SlotsSet], profile: &str, slots: u32, now: u64) -> u64 {
    since_set(
        set.iter()
            .filter(|event| event.profile == profile)
            .map(|event| (event.slots, event.at)),
        slots,
        now,
    )
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
    since_set(
        set.iter()
            .filter(|event| event.profile == profile)
            .map(|event| (event.cpus, event.at)),
        cpus,
        now,
    )
}

/// Since when, in ms, a profile's `events` (a value and when it was set, oldest first) have had `value`: the first
/// since the last with another. `now` when the latest has another, as before a reload announces it; 0 with none.
fn since_set(events: impl Iterator<Item = (u32, u64)>, value: u32, now: u64) -> u64 {
    let mut since = None;
    let mut seen = false;
    for (set, at) in events {
        seen = true;
        if set != value {
            since = None;
        } else if since.is_none() {
            since = Some(at);
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
