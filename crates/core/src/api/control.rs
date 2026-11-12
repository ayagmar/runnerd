//! The control endpoints' types: pausing with or without cancelling running jobs, what a refused request names,
//! the config file and its check, and the doctor's checks. Plain data; the engine and the daemon act on them.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::api::AdmissionPolicy;

/// A runner of this daemon that holds a slot, and the job it runs, if it took one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningJob {
    /// `owner/name` the runner is registered on.
    pub repository: String,
    /// The runner's name, also its container's.
    pub runner: String,
    /// The job's ID; `None` while the runner waits for a job or before its log shows which it took.
    pub job: Option<u64>,
}

impl fmt::Display for RunningJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.job {
            Some(job) => write!(f, "job {job} on {}", self.runner),
            None => write!(f, "{}", self.runner),
        }
    }
}

/// The body of a 409 refused because runners use what the request would change. Has the `message` of an
/// [`ErrorResponse`](super::ErrorResponse), so a client that reads only that still gets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Busy {
    /// What went wrong, naming the jobs.
    pub message: String,
    /// The runners in the way.
    pub jobs: Vec<RunningJob>,
}

/// `POST /v1/repositories/{owner}/{name}/pause` and `POST /v1/repositories/pause` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseQuery {
    /// Cancel the repository's running jobs and remove their runners. Without it, a pause while the repository
    /// runs a job is refused with [`Busy`].
    #[serde(default)]
    pub force: bool,
}

/// `POST /v1/repositories/{owner}/{name}/pause` and `POST /v1/repositories/pause`: the repository, or every one,
/// is paused, and stays paused when the daemon restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    /// The runners a forced pause stopped: those whose runs it cancelled go once they report the job cancelled,
    /// within 90 s, the others at once. Empty without `force`.
    pub stopped: Vec<RunningJob>,
}

/// `GET /v1/config`: the config the daemon runs with. Parse `text` with `config::Config::parse` for its fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFile {
    /// The file it was read from, and `PUT /v1/config` writes.
    pub path: String,
    /// The TOML, as last loaded or written by the daemon, with each `[[notify.targets]]` entry's `url` and header
    /// values shown as `config::SECRET_SHOWN`; `PUT /v1/config` puts them back.
    pub text: String,
    /// `config::revision` of `text`: send it as `PUT /v1/config?revision=` and a config replaced meanwhile, as
    /// by auto sizing, is not overwritten (409).
    pub revision: String,
}

/// `PUT /v1/config` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceQuery {
    /// The revision of the config the new text was made from; absent, the text replaces whatever runs.
    pub revision: Option<String>,
}

/// `POST /v1/config/check` and `PUT /v1/config`: what `runnerd check-config` prints for a config. `PUT` refuses
/// an invalid one with 422 and this body, which has an [`ErrorResponse`](super::ErrorResponse)'s `message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigCheck {
    /// Whether the daemon can run it.
    pub valid: bool,
    /// `valid, memory caps sum to …`, or why it is not valid.
    pub message: String,
    /// What a valid config should still fix.
    pub warnings: Vec<String>,
    /// `[host] admission`, which decides what must fit `MemoryMax`: `memory_caps` under caps, `memory_needed`
    /// under measured.
    pub admission: AdmissionPolicy,
    /// Bytes every slot and the mirror take at their caps, all at once, when it parses.
    pub memory_caps: Option<u64>,
    /// Bytes the admission rule must fit in `MemoryMax`, when it parses: `memory_caps` under caps; under measured,
    /// the largest single runner with the mirror.
    pub memory_needed: Option<u64>,
    /// The `MemoryMax` of the slice it names; `None` when unlimited or unknown.
    pub memory_max: Option<u64>,
    /// From `PUT /v1/config`, once written: the revision of the text written, to make the next change against.
    /// `None` from a check.
    pub revision: Option<String>,
}

/// `GET /v1/doctor`: what `runnerd doctor` checks, for the config the daemon runs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doctor {
    /// In the order a new user fixes them.
    pub checks: Vec<Check>,
    /// How current `checks` are (ADR 0132); absent from `runnerd doctor` and from a daemon before 0.2, which ran
    /// them for the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<DoctorState>,
}

/// How current the daemon's doctor checks are (ADR 0132). Times are seconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum DoctorState {
    /// No run has finished since the daemon started.
    NotRun,
    /// Run for the config the daemon runs with, recently enough to stand.
    Ready {
        /// When the run finished.
        at: u64,
    },
    /// Run for a config replaced since, or too long ago: they may no longer hold. A new run is under way or due.
    Stale {
        /// When the run finished.
        at: u64,
    },
    /// The last run did not finish.
    Failed {
        /// When it gave up.
        at: u64,
        /// Why.
        error: String,
    },
    /// A state from a newer daemon than this client knows: the checks are not known to be current.
    #[serde(other)]
    Unknown,
}

impl DoctorState {
    /// Why the checks are not to be read as current, as every client says it; `None` when they are.
    pub fn line(&self, now: u64) -> Option<String> {
        match self {
            DoctorState::Ready { .. } => None,
            DoctorState::NotRun => Some("Host checks have not finished yet".to_owned()),
            DoctorState::Stale { at } => Some(format!(
                "Host checks are from {} and may no longer hold; a new run is due",
                super::ago(*at, now)
            )),
            DoctorState::Failed { error, .. } => {
                Some(format!("Host checks could not finish: {error}"))
            }
            DoctorState::Unknown => {
                Some("Host checks are in a state this client does not know".to_owned())
            }
        }
    }
}

/// One thing the host needs to run runners. Its `Display` is the line `runnerd doctor` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Such as `Docker` or `sysbox`.
    pub name: String,
    /// The outcome.
    pub status: CheckStatus,
    /// What was found.
    pub detail: String,
    /// What to run or change to pass, for a failed check.
    pub fix: Option<String>,
}

/// How a [`Check`] came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    /// Ready.
    Pass,
    /// Runners cannot run until it is fixed.
    Fail,
    /// Works, but unsafe.
    Warn,
    /// Not checked, because a check it depends on failed or no config was given.
    Skip,
    /// For information.
    Info,
}

impl CheckStatus {
    /// Its label, as every client shows it: `ok`, `FAIL`, `WARN`, `skip` or `info`; a failure and a warning in
    /// capitals so they stand out uncoloured.
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Pass => "ok",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Warn => "WARN",
            CheckStatus::Skip => "skip",
            CheckStatus::Info => "info",
        }
    }

    /// What it means for the reader: a warning is not a failure.
    pub fn tone(self) -> super::Tone {
        match self {
            CheckStatus::Pass => super::Tone::Good,
            CheckStatus::Fail => super::Tone::Bad,
            CheckStatus::Warn => super::Tone::Warn,
            CheckStatus::Skip => super::Tone::Quiet,
            CheckStatus::Info => super::Tone::Neutral,
        }
    }
}

impl Doctor {
    /// Whether runners can run as far as the checks say: they are of a current run, or a daemon that says nothing of
    /// its run, and none failed. Warnings, skips and notes pass; checks that are not current never do.
    pub fn passed(&self) -> bool {
        matches!(self.state, None | Some(DoctorState::Ready { .. }))
            && Doctor::all_passed(&self.checks)
    }

    /// `Doctor::passed` of checks not yet in a `Doctor`, as `runnerd doctor` runs them.
    pub fn all_passed(checks: &[Check]) -> bool {
        !checks.iter().any(|check| check.status == CheckStatus::Fail)
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.status.label();
        write!(f, "{status:<5} {}: {}", self.name, self.detail)?;
        if let Some(fix) = &self.fix {
            for (i, line) in fix.lines().enumerate() {
                let label = if i == 0 { "fix:" } else { "" };
                write!(f, "\n      {label:<4} {line}")?;
            }
        }
        Ok(())
    }
}
