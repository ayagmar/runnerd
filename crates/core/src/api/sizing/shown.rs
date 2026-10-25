//! What the sizing advice means as every client shows it: what a set of jobs needs and why, how the advice reads at
//! a glance, what auto sizing does next with a profile at a given time, how much history it covers, and doctor's
//! check of it. Plain derivations of [`Sizing`]; the daemon's advice uses [`need`] for its arithmetic.

use super::{
    Advice, AutoPlan, JobSizing, PAGE_CACHE_ROOM, Peaks, ProfileSizing, Sizing, SizingChange,
    SizingHold, Starts,
};
use crate::api::{Check, CheckStatus, Tone, Toned};
use crate::config::ByteSize;

/// Advice rounds memory up to a multiple of this: 256M.
pub const GRANULE: u64 = 256 << 20;

/// Seconds between two samples of a running job's container. Clients without a `Sizing` or `SampleSeries` that
/// says so, such as a job's figures in `runnerctl`, word their sampling caveat from it.
pub const SAMPLE_INTERVAL: u64 = 2;

/// What a set of jobs whose largest sampled peak is `peak` needs, with `margin` percent: the peak plus the margin, or
/// plus `PAGE_CACHE_ROOM` when that is more, rounded up to `GRANULE`. The largest, not a percentile: the cap kills a
/// job that goes over it, so a cap below a peak a job reached kills it again.
pub fn need(peak: u64, margin: u32) -> u64 {
    let room = (peak.saturating_mul(u64::from(margin)) / 100).max(PAGE_CACHE_ROOM);
    peak.saturating_add(room)
        .div_ceil(GRANULE)
        .saturating_mul(GRANULE)
        .max(GRANULE)
}

/// The caveat on a sampled peak, as every client words it: `memory peaks are sampled every 2 s; spikes shorter than
/// that do not show`.
pub fn sampled_note(interval: u64) -> String {
    format!("memory peaks are sampled every {interval} s; spikes shorter than that do not show")
}

/// What a set of jobs needs, as the advice sizes for it, from `Sizing::need`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// In bytes.
    pub memory: u64,
    /// How it comes from the largest peak: `its largest peak + 15%` or `its largest peak + 1G for page cache`.
    pub reason: String,
}

impl Sizing {
    /// What `peaks` need: the advice's memory, or with too few jobs for advice, what it would size for.
    pub fn need(&self, peaks: &Peaks, advice: Advice) -> Need {
        let memory = match advice {
            Advice::Keep { memory }
            | Advice::Lower { memory, .. }
            | Advice::Raise { memory, .. }
            | Advice::SliceTooSmall { memory } => memory,
            Advice::TooFew { .. } => need(peaks.memory.max, self.margin),
        };
        Need {
            memory,
            reason: self.need_reason(peaks),
        }
    }

    /// How the advice comes from `peaks`' largest: `its largest peak + 15%`, or `its largest peak + 1G for page
    /// cache` when that is more.
    pub(crate) fn need_reason(&self, peaks: &Peaks) -> String {
        if peaks.memory.max * u64::from(self.margin) / 100 >= PAGE_CACHE_ROOM {
            format!("its largest peak + {}%", self.margin)
        } else {
            format!(
                "its largest peak + {} for page cache",
                ByteSize(PAGE_CACHE_ROOM)
            )
        }
    }

    /// How the advice is made, as every client explains it: `peaks are sampled every 2 s, so a shorter spike goes
    /// unseen: advice sizes for the largest + 15%, at least 1G for page cache, rounded up to 256M, once a profile ran
    /// 10 jobs`.
    pub fn method_line(&self) -> String {
        format!(
            "peaks are sampled every {} s, so a shorter spike goes unseen: advice sizes for the largest + {}%, at \
             least {} for page cache, rounded up to {}, once a profile ran {} jobs",
            self.sample_interval,
            self.margin,
            ByteSize(PAGE_CACHE_ROOM),
            ByteSize(GRANULE),
            self.min_jobs
        )
    }

    /// The history it was computed from: `since` to when it was computed, however long ago that was.
    pub fn covered(&self) -> u64 {
        self.until.saturating_sub(self.since)
    }

    /// Doctor's check of it: a warning while a profile needs more memory than its cap, else the profiles whose
    /// advice changes something, or that all fit.
    pub fn doctor_check(&self) -> Check {
        let changes: Vec<_> = self
            .profiles
            .iter()
            .filter(|profile| {
                profile.advice.change().is_some()
                    || matches!(profile.advice, Advice::SliceTooSmall { .. })
            })
            .collect();
        let raise = changes
            .iter()
            .any(|profile| profile.advice.tone() == Tone::Bad);
        let detail = if changes.is_empty() {
            let too_few = self
                .profiles
                .iter()
                .any(|profile| matches!(profile.advice, Advice::TooFew { .. }));
            if too_few {
                format!(
                    "some profiles ran fewer than {} jobs; runnerctl sizing shows their peaks",
                    self.min_jobs
                )
            } else {
                format!(
                    "every profile's memory fits its jobs' largest peak + {}%, at least {} for page cache",
                    self.margin,
                    ByteSize(PAGE_CACHE_ROOM)
                )
            }
        } else {
            changes
                .iter()
                .map(|profile| format!("{}: {}", profile.name, self.advice_line(profile)))
                .collect::<Vec<_>>()
                .join("; ")
        };
        Check {
            name: "sizing".to_owned(),
            status: if raise {
                CheckStatus::Warn
            } else {
                CheckStatus::Info
            },
            detail,
            fix: (!changes.is_empty()).then(|| {
                changes
                    .iter()
                    .map(|profile| match profile.advice {
                        Advice::SliceTooSmall { .. } => format!(
                            "raise MemoryMax in the slice's unit file, sudo systemctl daemon-reload, then: \
                             runnerctl sizing apply {}",
                            profile.name
                        ),
                        _ => format!("runnerctl sizing apply {}", profile.name),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        }
    }
}

impl Advice {
    /// What it means for the reader: more memory needed is bad, a lowering worth a look, a fitting cap good, too
    /// few jobs to tell in the background.
    pub fn tone(&self) -> Tone {
        match self {
            Advice::Raise { .. } | Advice::SliceTooSmall { .. } => Tone::Bad,
            Advice::Lower { .. } => Tone::Warn,
            Advice::Keep { .. } => Tone::Good,
            Advice::TooFew { .. } => Tone::Quiet,
        }
    }

    /// It in a word or two, for a table cell: `too few`, `keep`, `grow slice`, or the new `9G × 2`.
    pub fn word(&self) -> String {
        match *self {
            Advice::TooFew { .. } => "too few".to_owned(),
            Advice::Keep { .. } => "keep".to_owned(),
            Advice::SliceTooSmall { .. } => "grow slice".to_owned(),
            Advice::Lower { memory, slots } | Advice::Raise { memory, slots } => {
                format!("{} × {slots}", ByteSize(memory))
            }
        }
    }
}

impl JobSizing {
    /// What its jobs need, for a table cell, and what that means: `max 3G` with too few runs, `9G > cap` past its
    /// profile's cap, else `6G`.
    pub fn needs(&self) -> Toned {
        let (text, tone) = match self.advice {
            Advice::TooFew { .. } => (
                format!("max {}", ByteSize(self.peaks.memory.max)),
                Tone::Quiet,
            ),
            Advice::Raise { memory, .. } | Advice::SliceTooSmall { memory } => {
                (format!("{} > cap", ByteSize(memory)), Tone::Bad)
            }
            Advice::Keep { memory } | Advice::Lower { memory, .. } => {
                (ByteSize(memory).to_string(), Tone::Neutral)
            }
        };
        Toned { text, tone }
    }
}

impl Starts {
    /// Every job counted: warm and cold starts.
    pub fn total(&self) -> u32 {
        self.warm + self.cold
    }
}

/// What auto sizing does next with one profile at a given time, from `AutoPlan::profile_plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfilePlan<'plan> {
    /// The next look makes this change, or would once `AutoPlan::blocked` is fixed.
    Next {
        /// The change.
        change: &'plan SizingChange,
        /// Whether `AutoPlan::blocked` keeps it from being made.
        blocked: bool,
    },
    /// A recent change holds it from any change until `hold.until`.
    Held {
        /// The hold.
        hold: &'plan SizingHold,
    },
    /// A recent change holds it from a lowering until `hold.lowered_after()`; a raise may come before.
    LowerHeld {
        /// The hold.
        hold: &'plan SizingHold,
    },
    /// Nothing: the advice asks for no change, and no hold is left.
    Nothing,
}

impl AutoPlan {
    /// What it does next with `profile` at `now`: a hold whose time has passed holds nothing, however old the plan.
    pub fn profile_plan(&self, profile: &str, now: u64) -> ProfilePlan<'_> {
        if let Some(change) = self.next.iter().find(|change| change.profile == profile) {
            return ProfilePlan::Next {
                change,
                blocked: self.blocked.is_some(),
            };
        }
        match self.held.iter().find(|hold| hold.profile == profile) {
            Some(hold) if now < hold.until => ProfilePlan::Held { hold },
            Some(hold) if now < hold.lowered_after() => ProfilePlan::LowerHeld { hold },
            _ => ProfilePlan::Nothing,
        }
    }
}

impl ProfilePlan<'_> {
    /// When what it says ends: the hold's end, for a held profile.
    pub fn until(&self) -> Option<u64> {
        match self {
            ProfilePlan::Held { hold } => Some(hold.until),
            ProfilePlan::LowerHeld { hold } => Some(hold.lowered_after()),
            ProfilePlan::Next { .. } | ProfilePlan::Nothing => None,
        }
    }

    /// What it does, as the lead of its line, before `until` when it has one: `Next look, within 5 minutes: memory
    /// 9G → 6.75G, slots 2`, `Once that is fixed: …`, `No change`, `No lowering` or `Nothing to change`.
    pub fn headline(&self, plan: &AutoPlan) -> String {
        match self {
            ProfilePlan::Next {
                change,
                blocked: true,
            } => format!("Once that is fixed: {}", change.line()),
            ProfilePlan::Next { change, .. } => format!(
                "Next look, within {}: {}",
                super::period(plan.every),
                change.line()
            ),
            ProfilePlan::Held { .. } => "No change".to_owned(),
            ProfilePlan::LowerHeld { .. } => "No lowering".to_owned(),
            ProfilePlan::Nothing => "Nothing to change".to_owned(),
        }
    }

    /// Why, after when the hold's change was made for a held profile: the change's reason, `the interval holds it
    /// until then`, `a raise may come before then`, or the advice's line.
    pub fn reason(&self, sizing: &Sizing, profile: &ProfileSizing) -> String {
        match self {
            ProfilePlan::Next { change, .. } => change.reason.clone(),
            ProfilePlan::Held { .. } => "the interval holds it until then".to_owned(),
            ProfilePlan::LowerHeld { .. } => "a raise may come before then".to_owned(),
            ProfilePlan::Nothing => sizing.advice_line(profile),
        }
    }

    /// When the change that holds it was made, for a held profile.
    pub fn changed(&self) -> Option<u64> {
        match self {
            ProfilePlan::Held { hold } | ProfilePlan::LowerHeld { hold } => Some(hold.changed),
            ProfilePlan::Next { .. } | ProfilePlan::Nothing => None,
        }
    }
}

#[cfg(test)]
mod tests;
