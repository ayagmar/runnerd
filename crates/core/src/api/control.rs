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
    /// by auto sizing, is not overwritten (409). `None` from a daemon before it.
    #[serde(default)]
    pub revision: Option<String>,
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
    /// under measured. Caps from a daemon before it said.
    #[serde(default)]
    pub admission: AdmissionPolicy,
    /// Bytes every slot and the mirror take at their caps, all at once, when it parses.
    pub memory_caps: Option<u64>,
    /// Bytes the admission rule must fit in `MemoryMax`, when it parses: `memory_caps` under caps; under measured,
    /// the largest single runner with the mirror. `None` from a daemon before it said.
    #[serde(default)]
    pub memory_needed: Option<u64>,
    /// The `MemoryMax` of the slice it names; `None` when unlimited or unknown.
    pub memory_max: Option<u64>,
    /// From `PUT /v1/config`, once written: the revision of the text written, to make the next change against.
    /// `None` from a check, or from a daemon before it.
    #[serde(default)]
    pub revision: Option<String>,
}

/// `GET /v1/doctor`: what `runnerd doctor` checks, for the config the daemon runs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doctor {
    /// In the order a new user fixes them.
    pub checks: Vec<Check>,
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

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = match self.status {
            CheckStatus::Pass => "ok",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Warn => "WARN",
            CheckStatus::Skip => "skip",
            CheckStatus::Info => "info",
        };
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
