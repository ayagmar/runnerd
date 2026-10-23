//! How a runner was admitted, as the job history keeps it for its job: the policy, what the runner reserved of
//! the slice's budget and what that went by, and the budget and the slice when it started. Sizes are bytes, times
//! seconds since the Unix epoch.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::config::ByteSize;

/// `[host] admission`, as a job's record and the budget name it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdmissionPolicy {
    /// Every runner reserves its memory cap.
    #[default]
    Caps,
    /// Every runner reserves its predicted peak.
    Measured,
}

/// What a runner reserved goes by, in the order measured admission falls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Basis {
    /// The job's own runs, plus the margin: with `[sizing] min_jobs` of them, their p95 peak; with fewer, their
    /// largest, once `admission::OWN_RUNS` of them succeeded, its profile has `min_jobs` runs and no other copy of
    /// the job is queued or running.
    Job {
        /// How many.
        runs: u64,
        /// Which of their peaks. Absent on the wire for the p95, so a reader without the field reads it as the
        /// basis it always was.
        #[serde(default, skip_serializing_if = "JobPeak::is_p95")]
        peak: JobPeak,
    },
    /// Too few runs of the job for `Job` but enough of its profile: the larger of the profile's p95 peak and the
    /// job's own largest, plus the margin.
    Profile {
        /// How many runs of the profile.
        runs: u64,
    },
    /// No run of the job yet but enough of its profile: the profile's largest peak plus the margin, since a new
    /// job may be its heaviest.
    ProfileMax {
        /// How many runs of the profile.
        runs: u64,
    },
    /// A run of the job went past what it reserved lately: the job's largest peak plus the margin.
    Overran {
        /// When the job is predicted from its p95 again.
        until: u64,
    },
    /// The cap: admission by caps, or too few runs of the job and of its profile.
    Cap,
    /// The cap: a run of the job peaked at its cap or was killed for memory lately.
    Unpredictable {
        /// When the job may be predicted again.
        until: u64,
    },
    /// The cap: measured admission had no recent sample of the slice's memory to check.
    Unmeasured,
}

/// Which of a job's own peaks `Basis::Job` goes by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobPeak {
    /// Their p95.
    #[default]
    P95,
    /// Their largest: fewer than `[sizing] min_jobs` runs.
    Largest,
}

impl JobPeak {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes a reference"
    )]
    fn is_p95(&self) -> bool {
        *self == JobPeak::P95
    }
}

/// How a job's runner was admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    /// The policy it was admitted by.
    pub policy: AdmissionPolicy,
    /// What it reserved of the budget: under measured admission the largest prediction of its repository's
    /// queued jobs, since GitHub may have given it any of them.
    pub reserved: u64,
    /// What that went by.
    pub basis: Basis,
    /// The budget the other runners, the mirror and any shell left before it started; `None` for an unlimited
    /// slice.
    pub free: Option<u64>,
    /// The slice's memory then, without page cache; `None` without a recent sample.
    pub slice: Option<u64>,
}

impl Admission {
    /// One line on how the runner was admitted, as `runnerctl job` and the dashboard say it, such as `admitted by
    /// measured peaks: reserved 1.25G, the p95 peak of 10 runs of its job + the margin; 10G of the budget was free,
    /// the slice at 2G`.
    pub fn line(&self) -> String {
        let size = |bytes: u64| ByteSize(bytes).to_string();
        let reserved = size(self.reserved);
        let why = match self.basis {
            Basis::Job {
                runs,
                peak: JobPeak::P95,
            } => {
                format!("reserved {reserved}, the p95 peak of {runs} runs of its job + the margin")
            }
            Basis::Job {
                runs,
                peak: JobPeak::Largest,
            } => {
                format!(
                    "reserved {reserved}, the largest peak of {runs} runs of its job + the margin"
                )
            }
            Basis::Profile { runs } => format!(
                "reserved {reserved}, from {runs} runs of its profile: its job had too few of its own"
            ),
            Basis::ProfileMax { runs } => format!(
                "reserved {reserved}, the largest peak of {runs} runs of its profile + the margin: its job had no \
                 runs of its own"
            ),
            Basis::Overran { .. } => format!(
                "reserved {reserved}, its job's largest peak + the margin: a run of it went past what it reserved \
                 lately"
            ),
            Basis::Cap if self.policy == AdmissionPolicy::Caps => {
                format!("reserved its {reserved} cap")
            }
            Basis::Cap => format!("reserved its {reserved} cap: too few runs to predict from"),
            Basis::Unpredictable { .. } => format!(
                "reserved its {reserved} cap: a run of its job peaked at its cap or was killed for memory lately"
            ),
            Basis::Unmeasured => {
                format!("reserved its {reserved} cap: no recent sample of the slice's memory")
            }
        };
        let policy = match self.policy {
            AdmissionPolicy::Caps => "caps",
            AdmissionPolicy::Measured => "measured peaks",
        };
        let mut line = format!("admitted by {policy}: {why}");
        if let Some(free) = self.free {
            let _ = write!(line, "; {} of the budget was free", size(free));
        }
        if let Some(slice) = self.slice {
            let _ = write!(line, ", the slice at {}", size(slice));
        }
        line
    }
}

// The wire format is JSON, which only the host and client features read.
#[cfg(all(test, any(feature = "host", feature = "client")))]
mod tests;
