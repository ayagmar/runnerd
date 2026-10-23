//! Right-sizing as clients read it: per profile and per job (repository, workflow and job name), the spread of the
//! job history's sampled memory and CPU peaks, how long jobs waited for a slot, how busy the slots were, and what
//! to set each profile's `memory` and `slots` to. Plain data; the daemon computes it from the history. Sizes are
//! bytes, CPU millicores, times seconds since the Unix epoch.

use serde::{Deserialize, Serialize};

use crate::config::ByteSize;

/// `GET /v1/sizing` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingQuery {
    /// Only jobs queued at or after this time; default 30 days ago.
    pub since: Option<u64>,
}

/// `GET /v1/sizing`: what the job history says each configured profile needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sizing {
    /// The start of the history read: `since`, or the first runner recorded after it.
    pub since: u64,
    /// When it was computed.
    pub until: u64,
    /// The percent added to a sampled peak, `[sizing] margin`: peaks are sampled every `sample_interval`
    /// seconds, so a shorter spike between two samples goes unseen.
    pub margin: u32,
    /// Seconds between two samples of a job's container.
    pub sample_interval: u64,
    /// The fewest jobs a profile or job needs before its percentiles become advice, `[sizing] min_jobs`.
    pub min_jobs: u32,
    /// The slice's `MemoryMax`; `None` when it is unlimited.
    pub memory_max: Option<u64>,
    /// Every configured profile, by name.
    pub profiles: Vec<ProfileSizing>,
}

/// One profile: its limits now, its jobs' peaks and waits, and the advice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSizing {
    /// The profile.
    pub name: String,
    /// Its `memory` now: each container's hard limit.
    pub memory: u64,
    /// Its `cpus` now.
    pub cpus: u32,
    /// Its `slots` now.
    pub slots: u32,
    /// Its jobs' peaks.
    pub peaks: Peaks,
    /// Seconds from queued to started.
    pub wait: Spread,
    /// The percent of the time since `since` its slots held a runner, over its `slots` now.
    pub busy: u32,
    /// What to set `memory` and `slots` to. Sized for its largest peak, so no job gets less than it used.
    pub advice: Advice,
    /// Each job that ran in it, by repository, workflow and name.
    pub jobs: Vec<JobSizing>,
}

/// The jobs of one name in one workflow of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSizing {
    /// `owner/name` on GitHub.
    pub repository: String,
    /// The workflow's name, when the history knew it.
    pub workflow: Option<String>,
    /// The job's name in the workflow.
    pub name: String,
    /// Its runs' peaks.
    pub peaks: Peaks,
    /// Seconds from queued to started.
    pub wait: Spread,
    /// What its profile's `memory` would be for this job alone, and the `slots` that would fit in the memory the
    /// profile's slots take now.
    pub advice: Advice,
}

/// The peaks of a set of jobs, each the highest of its samples.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peaks {
    /// How many jobs: the number of peaks the percentiles are taken over.
    pub jobs: u64,
    /// Of those, how many peaked within 5% of their container's memory cap: likely held back or killed by it, so
    /// they may need more than they show.
    pub at_cap: u64,
    /// Memory without page cache, in bytes.
    pub memory: Spread,
    /// CPU, in millicores.
    pub cpu: Spread,
}

/// Percentiles of a set of values, by nearest rank: each is one of the values, and with fewer than 100 values
/// `p99` is the largest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spread {
    /// The median.
    pub p50: u64,
    /// The 95th percentile.
    pub p95: u64,
    /// The 99th percentile.
    pub p99: u64,
    /// The largest.
    pub max: u64,
}

/// What to set a profile's `memory` and `slots` to. `memory` is the largest sampled peak plus the margin, rounded
/// up to 256M: the cap kills a job that goes over it, so it is sized for the heaviest run, not a percentile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Advice {
    /// Fewer than `min_jobs` jobs: too few to advise on.
    TooFew {
        /// How many there are.
        jobs: u64,
    },
    /// The cap fits, and lowering it would save less than a tenth or a run peaked at its cap.
    Keep {
        /// What the jobs need.
        memory: u64,
    },
    /// The cap is a tenth or more above what the jobs need, and no run peaked at its cap.
    Lower {
        /// The new `memory`.
        memory: u64,
        /// The new `slots`: as many as fit in the memory the slots take now when jobs waited for one, else the
        /// same.
        slots: u32,
    },
    /// The jobs need more than the cap.
    Raise {
        /// The new `memory`.
        memory: u64,
        /// The new `slots`: the same, or as many as the slice's `MemoryMax` still holds.
        slots: u32,
    },
    /// The jobs need more than the cap, and the slice's `MemoryMax` does not hold one slot of what they need: the
    /// slice has to grow first.
    SliceTooSmall {
        /// What the jobs need.
        memory: u64,
    },
}

impl Advice {
    /// The `memory` and `slots` to write, when they differ from the profile's.
    pub fn change(&self) -> Option<Resize> {
        match *self {
            Advice::Lower { memory, slots } | Advice::Raise { memory, slots } => {
                Some(Resize { memory, slots })
            }
            Advice::TooFew { .. } | Advice::Keep { .. } | Advice::SliceTooSmall { .. } => None,
        }
    }
}

/// A profile's new `memory` and `slots`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resize {
    /// Bytes.
    pub memory: u64,
    /// Slots.
    pub slots: u32,
}

impl Sizing {
    /// One line on what to do with a profile and why, as `runnerctl sizing`, the dashboard and `runnerd doctor`
    /// say it, such as `lower memory to 2.5G from 7G, its largest peak + 15%; 5 slots fit in the same 14G`.
    pub fn advice_line(&self, profile: &ProfileSizing) -> String {
        let size = |bytes: u64| ByteSize(bytes).to_string();
        let peaks = &profile.peaks;
        let need = format!("its largest peak + {}%", self.margin);
        let at_cap = format!(
            "{} of {} jobs peaked at their cap",
            peaks.at_cap, peaks.jobs
        );
        match profile.advice {
            Advice::TooFew { jobs } => format!(
                "{jobs} jobs, too few to advise on (it takes {}); largest peak {} of a {} cap",
                self.min_jobs,
                size(peaks.memory.max),
                size(profile.memory)
            ),
            Advice::Keep { .. } if peaks.at_cap > 0 => format!(
                "keep {}: {at_cap}, so they may need more than they show",
                size(profile.memory)
            ),
            Advice::Keep { memory } => {
                format!("keep {}: {need} is {}", size(profile.memory), size(memory))
            }
            Advice::Lower { memory, slots } => {
                let share = profile.memory * u64::from(profile.slots);
                let then = if slots > profile.slots {
                    format!("{slots} slots fit in the same {}", size(share))
                } else {
                    format!(
                        "frees {} of the budget",
                        size(share - memory * u64::from(slots))
                    )
                };
                format!(
                    "lower memory to {} from {}, {need}; {then}",
                    size(memory),
                    size(profile.memory)
                )
            }
            Advice::Raise { memory, slots } => {
                let why = if peaks.at_cap > 0 {
                    at_cap
                } else {
                    format!("{need} is above the {} cap", size(profile.memory))
                };
                let slots = if slots < profile.slots {
                    format!("; {slots} of its {} slots fit the slice", profile.slots)
                } else {
                    String::new()
                };
                format!("raise memory to {}: {why}{slots}", size(memory))
            }
            Advice::SliceTooSmall { memory } => format!(
                "needs {memory_needed} ({need}), more than the slice holds for one slot: raise the slice's \
                 MemoryMax first",
                memory_needed = size(memory)
            ),
        }
    }
}
