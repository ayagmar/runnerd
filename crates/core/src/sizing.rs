//! Right-sizing advice from the job history: the spread of each profile's and each job's sampled peaks, waits and
//! slot use, and the `memory` and `slots` to set. Pure: the store reads the history, the daemon serves the result
//! (`api::Sizing`). It never changes the config; a client applies advice through `PUT /v1/config`, which validates
//! it like any other config.

use std::collections::BTreeMap;

use crate::api::{self, Advice, Peaks, Spread};
use crate::config::{Admission, Config};
use crate::observe::metrics::SAMPLE_INTERVAL;

/// Advice rounds memory up to a multiple of this: 256M.
pub(crate) const GRANULE: u64 = 256 << 20;

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
    /// Its sampled memory peak, bytes, as `measure` counts memory.
    pub memory_peak: u64,
    /// Its sampled CPU peak, millicores.
    pub cpu_peak: u64,
    /// When it was queued.
    pub queued_at: u64,
    /// When it started; `None` for a job lost before.
    pub started_at: Option<u64>,
    /// When it finished, or was found lost.
    pub finished_at: Option<u64>,
    /// Whether the kernel killed a process in its container for memory.
    pub oom_killed: bool,
    /// Whether its container's memory reached its cap with nothing left to reclaim (`memory.events` `oom`); `None`
    /// when its events were not recorded.
    pub hit_cap: Option<bool>,
    /// What its sampled memory counts.
    pub measure: Measure,
    /// What measured admission reserved for it; `None` under caps or for an adopted runner.
    pub reserved: Option<u64>,
}

/// What a run's sampled memory counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// `memory.current` less `inactive_file`, as runners started before schema version 8 were sampled: page cache
    /// read twice (`active_file`) counts, so peaks overstate what a job needs.
    WithActiveFile,
    /// What the kernel cannot reclaim from the container: `memory.current` less the file LRU lists and
    /// `slab_reclaimable`.
    Unreclaimable,
}

impl Ran {
    /// Whether it met its container's `cap`: killed for memory, or at the cap with nothing left to reclaim. Without
    /// its container's events, whether its peak came within `AT_CAP` percent of the cap.
    pub fn met_cap(&self, cap: u64) -> bool {
        self.oom_killed
            || self.hit_cap.unwrap_or_else(|| {
                self.memory_peak.saturating_mul(100) >= cap.saturating_mul(AT_CAP)
            })
    }
}

/// One runner container's time in a slot. Times in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// Its profile.
    pub profile: String,
    /// When it started.
    pub started_at: u64,
    /// When it was removed; `None` while it runs.
    pub removed_at: Option<u64>,
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
/// admission policy budgets it.
pub fn advise(
    config: &Config,
    memory_max: Option<u64>,
    history: &History,
    window: Window,
) -> api::Sizing {
    let since = history
        .runners
        .iter()
        .map(|runner| runner.started_at)
        .min()
        .map_or(window.since, |first| first.max(window.since));
    let span = window.now.saturating_sub(since).max(1);
    let caps = config.memory_caps().map_or(u64::MAX, |caps| caps.0);
    let mirror = config.mirror_memory();
    let rules = &config.sizing;
    let profiles = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let jobs: Vec<&Ran> = history
                .jobs
                .iter()
                .filter(|job| job.profile == *name)
                .collect();
            let share = profile.memory.0.saturating_mul(u64::from(profile.slots));
            let held: u64 = history
                .runners
                .iter()
                .filter(|runner| runner.profile == *name)
                .map(|runner| {
                    let end = runner.removed_at.unwrap_or(window.now).min(window.now);
                    end.saturating_sub(runner.started_at.max(since))
                })
                .sum();
            let busy = u32::try_from(
                (held.saturating_mul(100) / span.saturating_mul(u64::from(profile.slots).max(1)))
                    .min(100),
            )
            .unwrap_or(100);
            let mut keys: BTreeMap<(&str, Option<&str>, &str), Vec<&Ran>> = BTreeMap::new();
            for job in &jobs {
                keys.entry((&job.repository, job.workflow.as_deref(), &job.name))
                    .or_default()
                    .push(job);
            }
            let wait = waits(&jobs);
            let sized = Sized {
                cap: profile.memory.0,
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
                margin: rules.margin,
                min_jobs: rules.min_jobs,
            };
            let job_sizings: Vec<api::JobSizing> = keys
                .into_iter()
                .map(|((repository, workflow, job_name), runs)| {
                    let peaks = peaks(&runs);
                    let wait = waits(&runs);
                    api::JobSizing {
                        repository: repository.to_owned(),
                        workflow: workflow.map(str::to_owned),
                        name: job_name.to_owned(),
                        advice: sized.advice(&peaks, wait.p95 >= WAITED || busy >= BUSY),
                        peaks,
                        wait,
                    }
                })
                .collect();
            let peaks = peaks(&jobs);
            api::ProfileSizing {
                name: name.clone(),
                memory: profile.memory.0,
                cpus: profile.cpus,
                slots: profile.slots,
                advice: sized.advice(&peaks, sized.wants_slots),
                peaks,
                wait,
                busy,
                wants_slots: sized.wants_slots,
                jobs: job_sizings,
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
        admission: match config.host.admission {
            Admission::Caps => api::AdmissionPolicy::Caps,
            Admission::Measured => api::AdmissionPolicy::Measured,
        },
        profiles,
    }
}

/// A profile's limits and the rules advice follows.
struct Sized {
    cap: u64,
    slots: u32,
    /// What its slots take of the budget now: `memory × slots`.
    share: u64,
    admission: Admission,
    /// The memory the slice holds for it, as `admission` budgets it: all its slots at their caps under `caps`, one
    /// runner under `measured`; `None` for an unlimited slice.
    room: Option<u64>,
    /// Whether its jobs waited for a slot at p95, or its slots were busy, enough to want more.
    wants_slots: bool,
    margin: u32,
    min_jobs: u32,
}

impl Sized {
    /// The memory a set of jobs needs: its largest peak plus the margin, rounded up to `GRANULE`. The largest, not
    /// a percentile: the cap kills a job that goes over it, so a cap below a peak a job reached kills it again.
    fn need(&self, peaks: &Peaks) -> u64 {
        let largest = peaks.memory.max;
        let need = largest.saturating_add(largest.saturating_mul(u64::from(self.margin)) / 100);
        need.div_ceil(GRANULE).saturating_mul(GRANULE).max(GRANULE)
    }

    fn advice(&self, peaks: &Peaks, wants_slots: bool) -> Advice {
        if peaks.jobs < u64::from(self.min_jobs) {
            return Advice::TooFew { jobs: peaks.jobs };
        }
        let need = self.need(peaks);
        let fit = |memory: u64| u32::try_from(memory / need).unwrap_or(u32::MAX);
        if need > self.cap {
            let slots = match (self.admission, self.room) {
                (Admission::Caps, Some(room)) => fit(room).min(self.slots),
                (Admission::Measured, Some(room)) if room < need => 0,
                (_, _) => self.slots,
            };
            if slots == 0 {
                return Advice::SliceTooSmall { memory: need };
            }
            return Advice::Raise {
                memory: need,
                slots,
            };
        }
        // A run at its cap, this one or a smaller one it had then, may have been held back or killed by it: its
        // peak is a floor, not what it needs, so the cap is never lowered under it.
        if peaks.at_cap == 0 && self.cap - need >= self.cap * WORTH_LOWERING / 100 {
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
}

fn peaks(jobs: &[&Ran]) -> Peaks {
    let at_cap = jobs
        .iter()
        .filter(|job| job.memory_cap.is_some_and(|cap| job.met_cap(cap)))
        .count();
    Peaks {
        jobs: jobs.len() as u64,
        at_cap: at_cap as u64,
        memory: spread(jobs.iter().map(|job| job.memory_peak).collect()),
        cpu: spread(jobs.iter().map(|job| job.cpu_peak).collect()),
    }
}

/// Seconds from queued to started of the jobs that started.
fn waits(jobs: &[&Ran]) -> Spread {
    spread(
        jobs.iter()
            .filter_map(|job| {
                job.started_at
                    .map(|at| at.saturating_sub(job.queued_at) / 1000)
            })
            .collect(),
    )
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

#[cfg(test)]
mod tests;
