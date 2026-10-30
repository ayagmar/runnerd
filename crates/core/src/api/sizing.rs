//! Right-sizing as clients read it: per profile and per job (repository, workflow and job name), the spread of the
//! job history's sampled memory and CPU peaks, how long jobs waited for a slot, how busy the slots were, and what
//! to set each profile's `memory` and `slots` to. Plain data; the daemon computes it from the history. Sizes are
//! bytes, CPU millicores, times seconds since the Unix epoch.

use serde::{Deserialize, Serialize};

use crate::api::AdmissionPolicy;
use crate::config::ByteSize;

mod shown;

pub use shown::{GRANULE, Need, ProfilePlan, SAMPLE_INTERVAL, need, sampled_note};

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
    pub admission: AdmissionPolicy,
    /// Every configured profile, by name.
    pub profiles: Vec<ProfileSizing>,
    /// Whether the daemon applies advice itself, `[sizing] auto`.
    pub auto: bool,
    /// The changes auto sizing made since `since`, oldest first.
    pub changes: Vec<SizingChange>,
    /// Under auto sizing, what it does next and when. `None` while it is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_plan: Option<AutoPlan>,
}

/// What auto sizing will do: its rules, the changes its next look would make as the history and config are now, and
/// the profiles a recent change holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoPlan {
    /// Seconds between two looks of the daemon's.
    pub every: u64,
    /// `[sizing] interval`: the least time between a change to one profile and one that lowers it, in seconds.
    pub interval: u64,
    /// `[sizing] raise_interval`: the least time between a change to one profile and one that only raises it, in
    /// seconds.
    pub raise_interval: u64,
    /// `[sizing] max_step`: the most one change moves a profile's `memory`, in percent of it.
    pub max_step: u32,
    /// The changes the next look would make, in the order it would make them; `at` is when this was worked out.
    /// None of them is made while `blocked` says why.
    pub next: Vec<SizingChange>,
    /// The profiles a change in the last `interval` holds from some change, the hold that ends first first.
    pub held: Vec<SizingHold>,
    /// Why no look changes anything until the owner acts, as when the file on disk differs from the config runnerd
    /// runs with. `None` when the next look may write `next`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    /// The profiles whose jobs the CPU cap holds back and whose `cpus` the next look cannot raise, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<SizingRefused>,
}

/// A raise of a profile's `cpus` its jobs ask for that auto sizing cannot make, as when it does not fit the CPU
/// budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingRefused {
    /// The profile.
    pub profile: String,
    /// Which job the cap holds back, and what keeps it from a raise.
    pub reason: String,
}

/// A profile auto sizing changes nothing in before `until`, and lowers nothing in before `lower_until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingHold {
    /// The profile.
    pub profile: String,
    /// When its last change was made.
    pub changed: u64,
    /// When it may be raised again, in seconds since the Unix epoch.
    pub until: u64,
    /// When it may be lowered again.
    pub lower_until: u64,
}

impl SizingHold {
    /// When it may be lowered again.
    pub fn lowered_after(&self) -> u64 {
        self.lower_until.max(self.until)
    }
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
    /// Seconds from queued to started, leaving out the time before the history's start and the time no runnerd
    /// ran (ADR 0081).
    pub wait: Spread,
    /// Its jobs by whether a warm runner took them, and how long each kind waited.
    pub starts: Starts,
    /// The percent of the time since `since` its slots held a runner for a job, from when the job was queued, over
    /// its `slots` now.
    pub busy: u32,
    /// Whether its jobs waited a minute or more for a slot at p95, or its slots were busy half the time: more slots
    /// would help.
    pub wants_slots: bool,
    /// What to set `memory` and `slots` to. Sized for its largest peak, so no job gets less than it used.
    pub advice: Advice,
    /// When the advice is to take back a lowering auto sizing made that went too far: what showed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<SizingRevert>,
    /// Each job that ran in it, by repository, workflow and name.
    pub jobs: Vec<JobSizing>,
}

/// A lowering of a profile's `memory` by auto sizing that went too far, which the advice takes back (ADR 0070).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingRevert {
    /// The `memory` it lowered to, in bytes.
    pub from: u64,
    /// The `memory` it lowered from, which the advice goes back to.
    pub to: u64,
    /// The job that showed it, as `owner/name workflow / job`.
    pub job: String,
    /// How.
    pub cause: RevertCause,
}

/// What showed a lowering went too far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum RevertCause {
    /// A run met the lowered cap.
    Met,
    /// The job, which succeeded before the lowering, failed this many times in a row at it, in different workflow
    /// runs and on more than one branch: a heap sized from the cgroup's limit fails without the kernel's limit.
    Failed {
        /// How many.
        runs: u32,
    },
}

/// A profile's jobs by whether their runner was already up when GitHub queued them (ADR 0050).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Starts {
    /// Jobs that started on a warm runner: one that started before the job was queued.
    pub warm: u32,
    /// Their median wait from queued to started, in seconds.
    pub warm_p50: u64,
    /// Jobs whose runner started after they were queued.
    pub cold: u32,
    /// Their median wait, in seconds.
    pub cold_p50: u64,
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
    /// Seconds from queued to started, leaving out the time before the history's start and the time no runnerd
    /// ran (ADR 0081).
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
    /// Of those, how many met their container's memory cap: its own limit failed reclaim (for a run recorded
    /// before that was told apart, a kill or a limit hit in the container, or without events a peak within 5% of it).
    /// They may need more than they show.
    pub at_cap: u64,
    /// How many of their jobs are on their cap: one of their runs met it or was killed for memory, and fewer than 3
    /// successful runs since were clear of it. No lowering while any is.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub on_cap: u64,
    /// Memory without page cache, in bytes.
    pub memory: Spread,
    /// CPU, in millicores, of the runs at the profile's `cpus` now: a run at a larger cap may have peaked past them.
    pub cpu: Spread,
    /// How many runs `cpu` is over: those of `jobs` at the profile's `cpus` now.
    #[serde(default)]
    pub cpu_runs: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(count: &u64) -> bool {
    *count == 0
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

/// What a cap keeps at least over the largest peak, for page cache: 1G. Peaks leave page cache out, and a container
/// whose memory past them leaves the kernel less than about 500M of page cache to reclaim at its cap reclaims badly,
/// which throttles its sockets and slows its downloads (ADR 0055).
pub const PAGE_CACHE_ROOM: u64 = 1 << 30;

/// What to set a profile's `memory` and `slots` to. `memory` is the largest sampled peak plus the margin, or plus
/// `PAGE_CACHE_ROOM` when that is more, rounded up to 256M: the cap kills a job that goes over it, so it is sized for
/// the heaviest run, not a percentile.
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
            Advice::Lower { memory, slots } | Advice::Raise { memory, slots } => Some(Resize {
                memory,
                slots,
                cpus: None,
            }),
            Advice::TooFew { .. } | Advice::Keep { .. } | Advice::SliceTooSmall { .. } => None,
        }
    }
}

/// A profile's new `memory`, `slots` and, when auto sizing changed them, `cpus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resize {
    /// Bytes.
    pub memory: u64,
    /// Slots.
    pub slots: u32,
    /// CPUs per slot; `None` leaves them as they are, as advice does and as auto sizing's changes recorded before it
    /// changed them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
}

impl AutoPlan {
    /// Its rules in words, as clients say them after `runnerd`: `looks every 5 minutes, raises a profile at most
    /// once every 1 hour and lowers it at most once every 24 hours, moving its memory and CPUs by at most 25% at a
    /// time`.
    pub fn rules(&self) -> String {
        if self.raise_interval == 0 || self.raise_interval >= self.interval {
            return format!(
                "looks every {} and changes a profile at most once every {}, moving its memory by at most {}% at a \
                 time",
                period(self.every),
                period(self.interval),
                self.max_step
            );
        }
        format!(
            "looks every {}, raises a profile at most once every {} and lowers it at most once every {}, moving its \
             memory and CPUs by at most {}% at a time",
            period(self.every),
            period(self.raise_interval),
            period(self.interval),
            self.max_step
        )
    }
}

/// A setting's length in words, as a config's periods read: `5 minutes`, `24 hours`, `90 s`.
pub fn period(seconds: u64) -> String {
    let plural = |count: u64, unit: &str| {
        if count == 1 {
            format!("{count} {unit}")
        } else {
            format!("{count} {unit}s")
        }
    };
    match seconds {
        0 => "0 s".to_owned(),
        _ if seconds.is_multiple_of(3600) => plural(seconds / 3600, "hour"),
        _ if seconds.is_multiple_of(60) => plural(seconds / 60, "minute"),
        _ => format!("{seconds} s"),
    }
}

impl SizingChange {
    /// What it changed, such as `memory 9G → 6.75G, slots 2`, and `cpus 12 → 15` when it changed them.
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
        let cpus = match (self.from.cpus, self.to.cpus) {
            (Some(from), Some(to)) if from != to => format!(", cpus {from} → {to}"),
            _ => String::new(),
        };
        if self.uncertain {
            format!("{memory}, {slots}{cpus} (may not have been written)")
        } else {
            format!("{memory}, {slots}{cpus}")
        }
    }
}

impl Sizing {
    /// One line on what to do with a profile and why, as `runnerctl sizing`, the dashboard and `runnerd doctor`
    /// say it, such as `lower memory to 2.5G from 7G, its largest peak + 15%; 5 slots fit in the same 14G`.
    /// Under `measured` admission, when jobs wanted more slots, it adds that `slots` is not bought with caps there, or
    /// with auto sizing on, that it adds them.
    pub fn advice_line(&self, profile: &ProfileSizing) -> String {
        let line = self.memory_line(profile);
        if self.admission == AdmissionPolicy::Measured
            && profile.wants_slots
            && !matches!(profile.advice, Advice::TooFew { .. })
        {
            let then = if self.auto {
                "auto sizing adds one at a time while every profile's slots fit the slice at their jobs' peaks"
            } else {
                "measured admission reserves predicted peaks, not caps, so more slots may fit: raise slots by hand"
            };
            format!(
                "{line}; jobs waited {}s for a slot (p95) and slots were busy {}%: {then}",
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
        let need = self.need_reason(peaks);
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
            Advice::Keep { .. } if peaks.on_cap > 0 => format!(
                "keep {}: {} of its jobs met their cap or were killed for memory, and fewer than 3 runs since ran \
                 clear of it",
                size(profile.memory),
                peaks.on_cap
            ),
            Advice::Keep { memory } if memory > profile.memory => format!(
                "keep {}: its recent runs need {}, but none is known to have run out of memory at {} since it was set",
                size(profile.memory),
                size(memory),
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
            Advice::Raise { memory, .. } if let Some(revert) = &profile.revert => {
                let how = match revert.cause {
                    RevertCause::Met => format!("met the {} cap", size(revert.from)),
                    RevertCause::Failed { runs } => {
                        format!("failed {runs} times in a row at {}", size(revert.from))
                    }
                };
                format!(
                    "raise memory back to {}: {} {how} since auto sizing lowered it from {}",
                    size(memory),
                    revert.job,
                    size(revert.to)
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
