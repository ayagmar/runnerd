//! Right-sizing advice from the job history: the spread of each profile's and each job's sampled peaks, waits and
//! slot use, and the `memory` and `slots` to set. Pure: the store reads the history, the daemon serves the result
//! (`api::Sizing`). It never changes the config: a client applies advice through `PUT /v1/config`, which validates
//! it like any other config, and under `[sizing] auto` the daemon applies what `auto::changes` picks the same way.

use crate::admission::Key;
use crate::admission::recent::Recent;
use crate::api::{self, Advice, Peaks, Spread};
use crate::config::{Admission, Config};
use crate::observe::metrics::SAMPLE_INTERVAL;

pub(crate) use crate::api::GRANULE;

/// Without its container's `memory.events`, a peak at or above this share of its cap, in percent, likely met it.
const AT_CAP: u64 = 95;

/// Lowering a cap is advised once it saves at least this share of it, in percent.
const WORTH_LOWERING: u64 = 10;

/// Jobs that waited this long at p95, in seconds, or slots busy this share of the time, in percent, want more
/// slots.
const WAITED: u64 = 60;
const BUSY: u32 = 50;

/// One finished job of this daemon's runners, as sizing reads it. Times in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name, when known.
    pub workflow: Option<String>,
    /// The job's name.
    pub name: String,
    /// The profile its runner ran in.
    pub profile: String,
    /// Its container's memory cap, when recorded.
    pub memory_cap: Option<u64>,
    /// Its container's CPU cap, millicores, when recorded.
    pub cpu_cap: Option<u64>,
    /// Its sampled memory peak, bytes, as `measure` counts memory.
    pub memory_peak: u64,
    /// Its sampled CPU peak, millicores.
    pub cpu_peak: u64,
    /// Its CPU time over its run time, millicores; `None` when not recorded.
    pub cpu_average: Option<u64>,
    /// When it was queued.
    pub queued_at: u64,
    /// When it started; `None` for a job lost before.
    pub started_at: Option<u64>,
    /// When its runner's container started: before `queued_at`, it was a warm runner.
    pub runner_started_at: u64,
    /// When it finished, or was found lost.
    pub finished_at: Option<u64>,
    /// Whether the kernel killed a process in its container for memory.
    pub oom_killed: bool,
    /// Whether its container's memory, or a descendant's, reached a limit with nothing left to reclaim
    /// (`memory.events` `oom`); `None` when its events were not recorded.
    pub hit_cap: Option<bool>,
    /// Whether its container's own limit failed reclaim (`memory.events.local` `oom`), not a descendant's; `None`
    /// when not recorded, as before schema 14.
    pub own_cap: Option<bool>,
    /// What its sampled memory counts.
    pub measure: Measure,
    /// Whether it completed with success: a failed or lost run may have stopped before its heaviest step.
    pub succeeded: bool,
    /// Whether it completed with `failure` or `timed_out`: not lost, not cancelled.
    pub failed: bool,
    /// Its workflow run on GitHub: a re-run of its failed jobs has the same.
    pub run_id: u64,
    /// The branch it ran for, when known.
    pub branch: Option<String>,
    /// What measured admission reserved for it; `None` under caps or for an adopted runner.
    pub reserved: Option<u64>,
    /// How long of its wait it waited because the host's memory could not take its runner, in ms: no more slots
    /// would have started it sooner.
    pub host_waited: u64,
    /// How much of its wait passed while no runnerd ran, in ms.
    pub unseen: u64,
    /// The periods of its CPU quota it had work in (`api::JobMetrics::cpu_periods`); `None` where the kernel kept no
    /// figure, as in runs from before schema 17.
    pub cpu_periods: Option<u64>,
    /// Of those, the periods its quota throttled it in (`api::JobMetrics::cpu_throttled`).
    pub cpu_throttled: Option<u64>,
}

/// A job's wait for a runner as sizing, insights and the history's figures count it: from when GitHub queued it, or
/// from when the history holds every job if that came later (`history_kept`), as for a job queued before runnerd
/// first ran, to when it started, less what of it passed while no runnerd ran (ADR 0081). A job's own page shows its
/// whole wait, as GitHub's does; arrivals, as `simulate` and warm runners model them, go by `queued_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    /// When GitHub queued it, in ms.
    pub queued_at: u64,
    /// When it started, in ms.
    pub started_at: u64,
    /// How much of it passed while no runnerd ran, in ms.
    pub unseen: u64,
}

impl Wait {
    /// In ms, 0 at least, with the history holding every job since `kept_since` (ms).
    pub fn counted(self, kept_since: u64) -> u64 {
        self.started_at
            .saturating_sub(self.queued_at.max(kept_since))
            .saturating_sub(self.unseen)
    }
}

/// A run its CPU quota throttled in this share of its run or more, in percent, was held back by its cap: it ran as
/// fast as the cap let it, so its average says what the cap allowed. No lowering reads it, under `measured`
/// admission it raises the cap, and insights name its job (ADR 0079). The share is of the run's wall time in 100 ms
/// periods, not of the periods the kernel counted, which leave out the time it was idle: a job that waits on the
/// network for most of its run and bursts past its cap in a few compiles is throttled in most of its busy periods and
/// in little of its run. A throttled period loses only part of its 100 ms, so the share is more than the time the cap
/// cost. The Android quality gate lost about 90 s of its 517 at 9 CPUs, 17% of its run, and was throttled in 28% of
/// it, 21% at 13 and 16% at 19; an 8-minute job whose three 20-second compiles want 20 CPUs is throttled in 11%, and
/// lost about 6%.
pub const HELD: u64 = 15;

/// What a run's CPU quota did to it, as auto sizing and insights read it (ADR 0079). Times in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttling {
    /// When it started.
    pub started_at: Option<u64>,
    /// When it finished.
    pub finished_at: Option<u64>,
    /// The periods of its CPU quota it had work in (`api::JobMetrics::cpu_periods`); `None` where the kernel kept no
    /// figure, as in runs from before schema 17.
    pub periods: Option<u64>,
    /// Of those, the periods its quota throttled it in (`api::JobMetrics::cpu_throttled`).
    pub throttled: Option<u64>,
}

impl Throttling {
    /// How long it ran, from its start to its finish; `None` without either.
    pub fn took(self) -> Option<u64> {
        Some(self.finished_at?.saturating_sub(self.started_at?))
    }

    /// The share of its run its quota throttled it in, per mille: its throttled periods over its run's wall time in
    /// 100 ms periods, or over the periods the kernel counted when those are more, as when its start or finish is
    /// unknown. `None` without a throttling figure.
    pub fn share(self) -> Option<u64> {
        let (periods, throttled) = self.periods.zip(self.throttled)?;
        let run = self.took().map_or(0, |took| took / 100).max(periods);
        Some(throttled.saturating_mul(1000) / run.max(1))
    }

    /// Whether its cap held it back: throttled in `HELD` percent of its run or more. `None` without a throttling
    /// figure: unknown, never read as free.
    pub fn held(self) -> Option<bool> {
        let share = self.share()?;
        Some(self.throttled.is_some_and(|throttled| throttled > 0) && share >= HELD * 10)
    }
}

/// What a run's sampled memory counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// `memory.current` less `inactive_file`, as runners started before schema version 8 were sampled: page cache
    /// read twice (`active_file`) counts, so peaks overstate what a job needs.
    WithActiveFile,
    /// `memory.current` less the file LRU lists (`active_file`, `inactive_file`): an estimate of what a job holds
    /// without the page cache the kernel can drop.
    WithoutPageCache,
}

/// What a run says about its container's memory cap (ADR 0070).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capped {
    /// The container's own limit failed reclaim: it needed more than the cap.
    Met,
    /// Recorded without the local counter: by the reading before it, a kill or a limit hit in the container, or
    /// without events a peak within `AT_CAP` percent of the cap. Holds and floors as `Met`, never raises a cap.
    Assumed,
    /// A process in it was killed for memory while its own limit held: the slice or the host ran out.
    Killed,
    /// None of those. A peak near the cap with no `oom` at it is clear: a JVM fills the room it has.
    Clear,
}

impl Capped {
    /// Whether it says the job needed its cap: `Met` or `Assumed`.
    pub fn met(self) -> bool {
        matches!(self, Capped::Met | Capped::Assumed)
    }

    /// Whether it puts its job on its cap: anything but `Clear`.
    pub fn holds(self) -> bool {
        self != Capped::Clear
    }
}

impl Ran {
    /// Its wait, once it started.
    pub fn wait(&self) -> Option<Wait> {
        Some(Wait {
            queued_at: self.queued_at,
            started_at: self.started_at?,
            unseen: self.unseen,
        })
    }

    /// What its CPU quota did to it.
    pub fn throttling(&self) -> Throttling {
        Throttling {
            started_at: self.started_at,
            finished_at: self.finished_at,
            periods: self.cpu_periods,
            throttled: self.cpu_throttled,
        }
    }

    /// What it says about its container's `cap`. A kill that only a descendant's own limit explains, the counters
    /// showing a limit hit below the container and none at it, is a service container the job capped itself: clear.
    /// A kill no sample saw an `oom` before, its last sample within `AT_CAP` percent of the cap, met it in the last
    /// interval.
    pub fn capped(&self, cap: u64) -> Capped {
        match self.own_cap {
            Some(true) => Capped::Met,
            // No sample saw `oom` at any level and its last came near the cap: its own limit killed it before the
            // next sample could count it.
            Some(false)
                if self.oom_killed
                    && self.hit_cap == Some(false)
                    && self.memory_peak.saturating_mul(100) >= cap.saturating_mul(AT_CAP) =>
            {
                Capped::Met
            }
            Some(false) if self.oom_killed && self.hit_cap != Some(true) => Capped::Killed,
            None if self.oom_killed
                || self.hit_cap.unwrap_or_else(|| {
                    self.memory_peak.saturating_mul(100) >= cap.saturating_mul(AT_CAP)
                }) =>
            {
                Capped::Assumed
            }
            Some(false) | None => Capped::Clear,
        }
    }
}

/// One runner container's time in a slot for a job. Times in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// Its profile.
    pub profile: String,
    /// When it started holding the slot for a job: its start, or when its job was queued if that came later, as for
    /// a warm runner.
    pub started_at: u64,
    /// When it was removed; `None` while it runs.
    pub removed_at: Option<u64>,
}

/// A profile's `cpus` as the daemon ran it from `at` (ms), by its `profile.changed` event: one when it started with
/// the profile, and after each reload or restart that changed the profile's image, CPUs, memory or slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpusSet {
    /// The profile.
    pub profile: String,
    /// Its `cpus` from then.
    pub cpus: u32,
    /// When, in milliseconds.
    pub at: u64,
}

/// A profile's `memory` as the daemon ran it from `at` (ms), by its `profile.changed` event, as `CpusSet` has its
/// `cpus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySet {
    /// The profile.
    pub profile: String,
    /// Its `memory` from then, in bytes.
    pub memory: u64,
    /// When, in milliseconds.
    pub at: u64,
}

/// What the store reads for sizing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// Finished jobs with a recorded peak, cancelled ones left out.
    pub jobs: Vec<Ran>,
    /// Cancelled jobs, for what admission learns from how they ended: a kill for memory, a peak at the cap or past
    /// what was reserved. Their peaks (0 when never sampled) are not read as a job's size.
    pub cancelled: Vec<Ran>,
    /// Runners that held a slot at some point after the history's start.
    pub runners: Vec<Held>,
    /// Auto sizing's changes, oldest first, from `CHANGES_READ` before now: what a revert reads (ADR 0070). Empty
    /// where nothing reads them, as for admission.
    pub changes: Vec<api::SizingChange>,
    /// Each profile's `memory` as the daemon ran it, oldest first: since when a raise reads runs at it.
    pub memory_set: Vec<MemorySet>,
    /// From when, in ms, the history holds every job (`history_kept`): a wait before it, as of a job queued before
    /// runnerd first ran, counts from then.
    pub kept_since: u64,
}

/// What `advise` sizes against besides the history.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    /// The start asked for, in milliseconds, at most `now`.
    pub since: u64,
    /// Now, in milliseconds.
    pub now: u64,
    /// Seconds between two samples of a container.
    pub sample_interval: u64,
}

impl Window {
    /// The window `GET /v1/sizing` reads: from `since` (seconds), default 30 days before `now` (seconds), and
    /// never after `now`.
    pub fn new(since: Option<u64>, now: u64) -> Window {
        Window {
            since: since
                .unwrap_or_else(|| now.saturating_sub(30 * 24 * 3600))
                .min(now)
                .saturating_mul(1000),
            now: now.saturating_mul(1000),
            sample_interval: SAMPLE_INTERVAL.as_secs(),
        }
    }
}

/// The advice for every profile of `config` from `history`, against the slice's `memory_max` as `config`'s
/// admission policy budgets it. Peaks, and the memory advice, go by each job's sizing window over the history's last
/// 30 days (`memory`, ADR 0070); `window.since` narrows only the waits, the busy share and the starts.
pub fn advise(
    config: &Config,
    memory_max: Option<u64>,
    history: &History,
    window: Window,
) -> api::Sizing {
    let since = first_runner(history, window.since);
    let caps = config.memory_caps().map_or(u64::MAX, |caps| caps.0);
    let mirror = config.mirror_memory();
    let rules = &config.sizing;
    let recent = memory::windows(config, history, window.now);
    // Down to a granule, so the config file says it exactly and the check never reads more back.
    let most = config.runner_ceiling().map(|most| most / GRANULE * GRANULE);
    let profiles = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let jobs: Vec<&Ran> = history
                .jobs
                .iter()
                .filter(|job| job.profile == *name && job.queued_at >= window.since)
                .collect();
            let share = profile.memory.0.saturating_mul(u64::from(profile.slots));
            let busy = busy(history, name, profile.slots, since, window.now);
            let wait = waits(&jobs, history.kept_since);
            let starts = starts(&jobs, history.kept_since);
            let cap = profile.memory.0;
            let took = memory::took(&history.memory_set, name, cap, window.now);
            let sized = Sized {
                cap,
                cpus: u64::from(profile.cpus) * 1000,
                slots: profile.slots,
                share,
                admission: config.host.admission,
                room: memory_max.map(|max| match config.host.admission {
                    // What the slice leaves this profile once the other profiles and the mirror took theirs.
                    Admission::Caps => max.saturating_sub(caps.saturating_sub(share)),
                    // Measured admission reserves predicted peaks, not caps: the config check only holds one
                    // runner of each profile beside the mirror.
                    Admission::Measured => max.saturating_sub(mirror),
                }),
                wants_slots: wait.p95 >= WAITED || busy >= BUSY,
                min_jobs: rules.min_jobs,
                ceiling: most,
            };
            let raises = Raises {
                history,
                recent: &recent,
                profile: name,
                cap,
                took,
                margin: rules.margin,
            };
            let memory = job_memory(&raises, &jobs, &sized, window.now, busy);
            let peaks = memory::peaks(
                recent.jobs_of(name).flat_map(|(_, runs)| runs.window()),
                memory.on_cap,
                sized.cpus,
            );
            let current = api::Resize {
                memory: cap,
                slots: profile.slots,
                cpus: None,
            };
            let revert = memory::revert(config, history, name, current);
            let (advice, wanted) = if let Some((_, to)) = &revert {
                (sized.reverted(*to), to.memory)
            } else {
                let needs = Needs {
                    raise: raises.need(None),
                    floor: memory.floor.max(memory::revert_floor(history, name)),
                };
                (sized.advice(&peaks, needs, sized.wants_slots), needs.raise)
            };
            let ceiling = ceiling_hold(config, most, advice, wanted);
            api::ProfileSizing {
                name: name.clone(),
                memory: cap,
                cpus: profile.cpus,
                slots: profile.slots,
                advice,
                revert: revert.map(|(revert, _)| revert),
                ceiling,
                peaks,
                wait,
                starts,
                busy,
                wants_slots: sized.wants_slots,
                jobs: memory.jobs,
            }
        })
        .collect();
    api::Sizing {
        since: since / 1000,
        until: window.now / 1000,
        margin: rules.margin,
        sample_interval: window.sample_interval,
        min_jobs: rules.min_jobs,
        memory_max,
        admission: config.host.admission.policy(),
        profiles,
        auto: config.sizing.auto,
        changes: Vec::new(),
        auto_plan: None,
    }
}

/// What says `[host] memory_ceiling` holds `advice` under the `wanted` memory, when it does: a raise or no raise,
/// and more wanted than `most`, the most a raise goes to.
fn ceiling_hold(
    config: &Config,
    most: Option<u64>,
    advice: Advice,
    wanted: u64,
) -> Option<api::CeilingHold> {
    let raises = matches!(advice, Advice::Raise { .. } | Advice::SliceTooSmall { .. });
    most.filter(|most| raises && wanted > *most)
        .zip(config.host.memory_ceiling)
        .map(|(most, ceiling)| api::CeilingHold {
            need: wanted,
            ceiling: ceiling.0,
            mirror: config.mirror_memory(),
            most,
        })
}

/// What a raise of one profile reads (`memory::raise_need`).
struct Raises<'history> {
    history: &'history History,
    recent: &'history Recent,
    profile: &'history str,
    /// Its `memory`.
    cap: u64,
    /// Since when it has had it, in ms.
    took: u64,
    margin: u32,
}

impl Raises<'_> {
    /// What a raise needs from the job `key`, or from every job for `None`.
    fn need(&self, key: Option<&Key>) -> u64 {
        memory::raise_need(
            self.history,
            self.recent,
            self.profile,
            self.cap,
            self.took,
            self.margin,
            key,
        )
    }
}

/// What a profile's jobs say about its `memory`.
struct JobMemory {
    /// Each job's sizing.
    jobs: Vec<api::JobSizing>,
    /// How many of them are on their cap.
    on_cap: u64,
    /// The largest of their floors.
    floor: u64,
}

/// Each job of `raises.profile` with runs in its window: its peaks, waits and advice; `jobs` are the profile's runs
/// since the window's start, for the waits.
fn job_memory(raises: &Raises<'_>, jobs: &[&Ran], sized: &Sized, now: u64, busy: u32) -> JobMemory {
    let mut on_cap = 0;
    let mut floor = 0;
    let sizings = raises
        .recent
        .jobs_of(raises.profile)
        .filter(|(_, runs)| runs.runs() > 0)
        .map(|(key, runs)| {
            let held = memory::on_cap(runs, now);
            on_cap += u64::from(held);
            let peaks = memory::peaks(runs.window(), u64::from(held), sized.cpus);
            let needs = Needs {
                raise: raises.need(Some(key)),
                floor: memory::floor(runs, raises.margin),
            };
            floor = floor.max(needs.floor);
            let runs_of: Vec<&Ran> = jobs
                .iter()
                .copied()
                .filter(|job| Key::of(job) == *key)
                .collect();
            let wait = waits(&runs_of, raises.history.kept_since);
            // A job's advice says what it needs; only its profile's is bounded by what can be written.
            let unbounded = Sized {
                ceiling: None,
                ..*sized
            };
            api::JobSizing {
                repository: key.repository.clone(),
                workflow: key.workflow.clone(),
                name: key.name.clone(),
                advice: unbounded.advice(&peaks, needs, wait.p95 >= WAITED || busy >= BUSY),
                peaks,
                wait,
            }
        })
        .collect();
    JobMemory {
        jobs: sizings,
        on_cap,
        floor,
    }
}

/// Where a history read from `start` (ms) begins: at its first runner when that started later, as a new daemon's
/// history does.
fn first_runner(history: &History, start: u64) -> u64 {
    history
        .runners
        .iter()
        .map(|runner| runner.started_at)
        .min()
        .map_or(start, |first| first.max(start))
}

/// The percent of the time from `since` to `now` (ms) that the profile's runners held its `slots`.
fn busy(history: &History, profile: &str, slots: u32, since: u64, now: u64) -> u32 {
    let span = now.saturating_sub(since).max(1);
    let held: u64 = history
        .runners
        .iter()
        .filter(|runner| runner.profile == profile)
        .map(|runner| {
            let end = runner.removed_at.unwrap_or(now).min(now);
            end.saturating_sub(runner.started_at.max(since))
        })
        .sum();
    u32::try_from(
        (held.saturating_mul(100) / span.saturating_mul(u64::from(slots).max(1))).min(100),
    )
    .unwrap_or(100)
}

/// A profile's limits and the rules advice follows.
#[derive(Clone, Copy)]
struct Sized {
    cap: u64,
    /// Its `cpus`, in millicores: CPU peaks are read only of runs at them.
    cpus: u64,
    slots: u32,
    /// What its slots take of the budget now: `memory × slots`.
    share: u64,
    admission: Admission,
    /// The memory the slice holds for it, as `admission` budgets it: all its slots at their caps under `caps`, one
    /// runner under `measured`; `None` for an unlimited slice.
    room: Option<u64>,
    /// Whether its jobs waited for a slot at p95, or its slots were busy, enough to want more.
    wants_slots: bool,
    min_jobs: u32,
    /// The most `memory` a raise goes to under `[host] memory_ceiling` (`Config::runner_ceiling`), down to a
    /// granule; `None` without a ceiling.
    ceiling: Option<u64>,
}

/// What a set of jobs' runs say its `memory` needs (`memory`, ADR 0070).
#[derive(Debug, Clone, Copy)]
struct Needs {
    /// What a raise goes to: from runs that met the current cap since the profile took it; 0 without one.
    raise: u64,
    /// What a lowering stays at or above: every windowed run's need, and any revert's floor.
    floor: u64,
}

impl Sized {
    /// A raise toward `needs.raise` when it is above the cap; a lowering toward `needs.floor` when that saves a tenth
    /// and no job is on its cap; else keep, with `needs.floor` as what the jobs need, above the cap or not.
    fn advice(&self, peaks: &Peaks, needs: Needs, wants_slots: bool) -> Advice {
        if peaks.jobs < u64::from(self.min_jobs) {
            return Advice::TooFew { jobs: peaks.jobs };
        }
        if needs.raise > self.cap {
            let need = needs.raise;
            // A raise the config check would refuse is no raise: it would be proposed again at every look.
            let memory = self.ceiling.map_or(need, |most| need.min(most));
            if memory <= self.cap {
                return Advice::SliceTooSmall { memory: need };
            }
            let fit = |room: u64| u32::try_from(room / memory).unwrap_or(u32::MAX);
            let slots = match (self.admission, self.room) {
                (Admission::Caps, Some(room)) => fit(room).min(self.slots),
                (Admission::Measured, Some(room)) if room < memory => 0,
                (_, _) => self.slots,
            };
            if slots == 0 {
                return Advice::SliceTooSmall { memory: need };
            }
            return Advice::Raise { memory, slots };
        }
        let need = needs.floor.max(GRANULE);
        // A job on its cap may have been held back or killed by it: no lowering until it ran clear of it again.
        if peaks.on_cap == 0
            && need <= self.cap
            && self.cap - need >= self.cap * WORTH_LOWERING / 100
        {
            let fit = |memory: u64| u32::try_from(memory / need).unwrap_or(u32::MAX);
            // Under measured admission slots are not bought with caps: the advice line says when more may help.
            return Advice::Lower {
                memory: need,
                slots: if wants_slots && self.admission == Admission::Caps {
                    fit(self.share).max(self.slots)
                } else {
                    self.slots
                },
            };
        }
        Advice::Keep { memory: need }
    }

    /// The raise that takes back a lowering that went too far, to `to`: as far as `[host] memory_ceiling` allows,
    /// which a reload may have lowered since; while it allows nothing past the cap, no raise.
    fn reverted(&self, to: api::Resize) -> Advice {
        let memory = self.ceiling.map_or(to.memory, |most| to.memory.min(most));
        if memory <= self.cap {
            return Advice::SliceTooSmall { memory: to.memory };
        }
        Advice::Raise {
            memory,
            slots: to.slots,
        }
    }
}

/// The [waits](Wait) of the jobs that started, in seconds, less what they waited for the host's memory.
fn waits(jobs: &[&Ran], kept_since: u64) -> Spread {
    spread(
        jobs.iter()
            .filter_map(|job| {
                job.wait()
                    .map(|wait| wait.counted(kept_since).saturating_sub(job.host_waited) / 1000)
            })
            .collect(),
    )
}

/// The jobs that started on a warm runner and the others, and their median waits.
fn starts(jobs: &[&Ran], kept_since: u64) -> api::Starts {
    let (warm, cold): (Vec<&Ran>, Vec<&Ran>) = jobs
        .iter()
        .filter(|job| job.started_at.is_some())
        .partition(|job| job.runner_started_at < job.queued_at);
    let count = |jobs: &[&Ran]| u32::try_from(jobs.len()).unwrap_or(u32::MAX);
    api::Starts {
        warm: count(&warm),
        warm_p50: waits(&warm, kept_since).p50,
        cold: count(&cold),
        cold_p50: waits(&cold, kept_since).p50,
    }
}

/// Nearest-rank percentiles; all zero for no values.
pub(crate) fn spread(mut values: Vec<u64>) -> Spread {
    values.sort_unstable();
    let rank = |percent: usize| {
        let index = (values.len() * percent).div_ceil(100).saturating_sub(1);
        values.get(index).copied().unwrap_or(0)
    };
    Spread {
        p50: rank(50),
        p95: rank(95),
        p99: rank(99),
        max: values.last().copied().unwrap_or(0),
    }
}

pub mod auto;
pub mod memory;

#[cfg(test)]
mod tests;
