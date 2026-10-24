//! Right-sizing as clients read it: per profile and per job (repository, workflow and job name), the spread of the
//! job history's sampled memory and CPU peaks, how long jobs waited for a slot, how busy the slots were, and what
//! to set each profile's `memory` and `slots` to. Plain data; the daemon computes it from the history. Sizes are
//! bytes, CPU millicores, times seconds since the Unix epoch.

use serde::{Deserialize, Serialize};

use crate::api::AdmissionPolicy;
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
    /// `[host] admission`, which the advice budgets `MemoryMax` by.
    #[serde(default)]
    pub admission: AdmissionPolicy,
    /// Every configured profile, by name.
    pub profiles: Vec<ProfileSizing>,
    /// Whether the daemon applies advice itself, `[sizing] auto`.
    #[serde(default)]
    pub auto: bool,
    /// The changes auto sizing made since `since`, oldest first.
    #[serde(default)]
    pub changes: Vec<SizingChange>,
    /// Under auto sizing, what it does next and when. `None` while it is off, and from a daemon before it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_plan: Option<AutoPlan>,
}

/// What auto sizing will do: its rules, the changes its next look would make as the history and config are now, and
/// the profiles a recent change holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoPlan {
    /// Seconds between two looks of the daemon's.
    pub every: u64,
    /// `[sizing] interval`: the least time between two changes to one profile, in seconds.
    pub interval: u64,
    /// `[sizing] max_step`: the most one change moves a profile's `memory`, in percent of it.
    pub max_step: u32,
    /// The changes the next look would make, in the order it would make them; `at` is when this was worked out.
    /// None of them is made while `blocked` says why.
    pub next: Vec<SizingChange>,
    /// The profiles a change in the last `interval` holds, oldest hold first.
    pub held: Vec<SizingHold>,
    /// Why no look changes anything until the owner acts, as when the file on disk differs from the config runnerd
    /// runs with. `None` when the next look may write `next`, and from a daemon before it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// A profile auto sizing changes nothing in before `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingHold {
    /// The profile.
    pub profile: String,
    /// When its last change was made.
    pub changed: u64,
    /// When its interval ends, in seconds since the Unix epoch.
    pub until: u64,
}

/// A profile's `memory` and `slots` as auto sizing changed them in the config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingChange {
    /// When it was written.
    pub at: u64,
    /// The profile.
    pub profile: String,
    /// What they were: writing these back undoes it.
    pub from: Resize,
    /// What they are now.
    pub to: Resize,
    /// Why, as the advice said it then.
    pub reason: String,
    /// Whether it may not have been written: recorded before its file was written, and the config no longer said
    /// whether it was when runnerd looked again. It still holds the interval, and `from` is still what it replaced.
    #[serde(default)]
    pub uncertain: bool,
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
    /// Whether its jobs waited a minute or more for a slot at p95, or its slots were busy half the time: more slots
    /// would help.
    #[serde(default)]
    pub wants_slots: bool,
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
    /// Of those, how many met their container's memory cap: killed for memory, or out of memory at it with
    /// nothing left to reclaim (for a run whose container's events were not recorded, a peak within 5% of it). They
    /// may need more than they show.
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
    /// The cap fits, and lowering it would save less than a tenth or a run met its cap.
    Keep {
        /// What the jobs need.
        memory: u64,
    },
    /// The cap is a tenth or more above what the jobs need, and no run met its cap.
    Lower {
        /// The new `memory`.
        memory: u64,
        /// The new `slots`: under `caps` admission, as many as fit in the memory the slots take now when jobs waited
        /// for one; else the same.
        slots: u32,
    },
    /// The jobs need more than the cap.
    Raise {
        /// The new `memory`.
        memory: u64,
        /// The new `slots`: the same, or under `caps` admission as many as the slice's `MemoryMax` still holds.
        slots: u32,
    },
    /// The jobs need more than the cap, and the slice's `MemoryMax` does not hold one slot of what they need (under
    /// `measured` admission, one runner beside the mirror): the slice has to grow first.
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

impl SizingChange {
    /// What it changed, such as `memory 9G → 6.75G, slots 2`.
    pub fn line(&self) -> String {
        let memory = if self.from.memory == self.to.memory {
            format!("memory {}", ByteSize(self.to.memory))
        } else {
            format!(
                "memory {} → {}",
                ByteSize(self.from.memory),
                ByteSize(self.to.memory)
            )
        };
        let slots = if self.from.slots == self.to.slots {
            format!("slots {}", self.to.slots)
        } else {
            format!("slots {} → {}", self.from.slots, self.to.slots)
        };
        if self.uncertain {
            format!("{memory}, {slots} (may not have been written)")
        } else {
            format!("{memory}, {slots}")
        }
    }
}

impl Sizing {
    /// One line on what to do with a profile and why, as `runnerctl sizing`, the dashboard and `runnerd doctor`
    /// say it, such as `lower memory to 2.5G from 7G, its largest peak + 15%; 5 slots fit in the same 14G`.
    /// Under `measured` admission, when jobs wanted more slots, it adds that `slots` is not bought with caps there.
    pub fn advice_line(&self, profile: &ProfileSizing) -> String {
        let line = self.memory_line(profile);
        if self.admission == AdmissionPolicy::Measured
            && profile.wants_slots
            && !matches!(profile.advice, Advice::TooFew { .. })
        {
            format!(
                "{line}; jobs waited {}s for a slot (p95) and slots were busy {}%: measured admission reserves \
                 predicted peaks, not caps, so more slots may fit: raise slots by hand",
                profile.wait.p95, profile.busy
            )
        } else {
            line
        }
    }

    /// `advice_line` without what it says about slots under measured admission.
    pub(crate) fn memory_line(&self, profile: &ProfileSizing) -> String {
        let size = |bytes: u64| ByteSize(bytes).to_string();
        let peaks = &profile.peaks;
        let need = format!("its largest peak + {}%", self.margin);
        let at_cap = format!(
            "{} of {} jobs ran out of memory at their cap",
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
                if self.admission == AdmissionPolicy::Measured {
                    return format!(
                        "lower memory to {} from {}, {need}",
                        size(memory),
                        size(profile.memory)
                    );
                }
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

// The wire format is JSON, which only the host and client features read.
#[cfg(all(test, any(feature = "host", feature = "client")))]
mod tests;
