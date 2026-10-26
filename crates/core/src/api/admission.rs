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
    /// A run of the job among its recent runs went past what it reserved: the job's largest peak plus the margin.
    Overran {
        /// When the job is predicted from its p95 again at the latest: sooner once that run leaves its recent runs.
        until: u64,
    },
    /// The cap: admission by caps, or too few runs of the job and of its profile.
    Cap,
    /// The cap: a run of the job ran out of memory at its cap or was killed for memory, and fewer than 3 of its runs
    /// since succeeded clear of it.
    Unpredictable {
        /// When the job may be predicted again at the latest: sooner after 3 clear runs.
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
    /// The slice's memory then, less its inactive page cache (`HostSample::memory`); `None` without a recent sample.
    pub slice: Option<u64>,
    /// What it reserved of the CPU budget, under `[host] cpu_overcommit`. Absent without one, and from a daemon
    /// before CPU admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuAdmission>,
}

/// What a runner reserved of the CPU budget. CPU in millicores, times in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuAdmission {
    /// The CPU it reserved: its profile's `cpus` under caps, its predicted average CPU under measured admission.
    pub reserved: u64,
    /// What that went by: `Basis::Job`, `Profile`, `ProfileMax` or `Cap`.
    pub basis: Basis,
    /// The CPU budget the other runners left before it started.
    pub free: u64,
    /// How long its repository's next job waited for CPU that memory and a slot had room for; absent when it
    /// did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waited: Option<u64>,
}

impl Admission {
    /// One line on how the runner was admitted, as `runnerctl job` and the dashboard say it, such as `admitted by
    /// measured peaks: reserved 1.25G, the p95 peak of its job's last 10 runs + the margin; 10G of the budget was free,
    /// the slice at 2G`.
    pub fn line(&self) -> String {
        let size = |bytes: u64| ByteSize(bytes).to_string();
        let reserved = size(self.reserved);
        let why = match self.basis {
            Basis::Job {
                runs,
                peak: JobPeak::P95,
            } => {
                format!(
                    "reserved {reserved}, the p95 peak of its job's last {runs} runs + the margin"
                )
            }
            Basis::Job {
                runs,
                peak: JobPeak::Largest,
            } => {
                format!(
                    "reserved {reserved}, the largest peak of its job's last {runs} runs + the margin"
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
                "reserved its {reserved} cap: a run of its job ran out of memory at its cap or was killed for memory lately"
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
        if let Some(cpu) = self.cpu {
            let _ = write!(line, ". {}", cpu.line());
        }
        line
    }
}

impl CpuAdmission {
    /// One sentence on what the runner reserved of the CPU budget, as `Admission::line` ends, such as `Reserved 2.5
    /// CPUs, the p95 average of 10 runs of its job; 20 CPUs of the budget were free; it waited 1m 30s for CPU`.
    pub fn line(&self) -> String {
        let reserved = cpus(self.reserved);
        let why = match self.basis {
            Basis::Job {
                runs,
                peak: JobPeak::P95,
            } => format!("the p95 average of {runs} runs of its job"),
            Basis::Job {
                runs,
                peak: JobPeak::Largest,
            } => format!("the largest average of {runs} runs of its job"),
            Basis::Profile { runs } => {
                format!("from {runs} runs of its profile: its job had too few of its own")
            }
            Basis::ProfileMax { runs } => format!(
                "the largest average of {runs} runs of its profile: its job had no runs of its own"
            ),
            Basis::Cap
            | Basis::Overran { .. }
            | Basis::Unpredictable { .. }
            | Basis::Unmeasured => "its profile's cpus".to_owned(),
        };
        let mut line = format!(
            "Reserved {reserved}, {why}; {} of the budget were free",
            cpus(self.free)
        );
        if let Some(waited) = self.waited {
            let _ = write!(line, "; it waited {} for CPU", super::duration(waited));
        }
        line
    }
}

/// Millicores as a number of CPUs to a tenth, rounded up: `2.5`, `32`.
pub fn cores(millicores: u64) -> String {
    tenths(millicores.div_ceil(100))
}

/// Millicores as `cores` writes them, to a tenth rounded down, so free CPU never reads as more than it is.
pub(super) fn free_cores(millicores: u64) -> String {
    tenths(millicores / 100)
}

fn tenths(tenths: u64) -> String {
    if tenths.is_multiple_of(10) {
        (tenths / 10).to_string()
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

/// Millicores as `2.5 CPUs` or `1 CPU`, to a tenth rounded up.
pub fn cpus(millicores: u64) -> String {
    match cores(millicores).as_str() {
        "1" => "1 CPU".to_owned(),
        number => format!("{number} CPUs"),
    }
}

// The wire format is JSON, which only the host and client features read.
#[cfg(all(test, any(feature = "host", feature = "client")))]
mod tests;
