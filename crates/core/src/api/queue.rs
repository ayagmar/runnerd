//! What holds the queue as a whole, for a script to ask in one call: whether a queued job waits for the host's
//! memory; and what a job's wait is counted under in the history, [`WaitCause`] (ADR 0154). It reads the status the
//! daemon served; it decides nothing about scheduling.

use serde::{Deserialize, Serialize};

use super::{QueueShown, QueuedJob, RepositoryStatus, Status, Waiting};

/// What a queued job's wait is counted under in the history, one per [`Waiting`] reason or a few of them together
/// (ADR 0154): each plan that holds a job adds the time to the next plan to its cause. Stored by [`WaitCause::word`],
/// so a cause a later runnerd adds reads as [`WaitCause::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WaitCause {
    /// Behind an older job that waited long enough for memory or CPU that no younger one starts before it
    /// (`Waiting::BehindMemory`, `Waiting::BehindCpu`).
    Barrier,
    /// Behind an older job's runner starting first (`Waiting::BehindStart`).
    Start,
    /// Every slot of its profile held a runner (`Waiting::NoSlot`).
    Slot,
    /// Its runner did not fit the slice's memory budget (`Waiting::Memory`).
    Budget,
    /// Its runner did not fit what the host's memory has to spare (`Waiting::HostMemory`).
    Host,
    /// Its runner did not fit the CPU budget (`Waiting::Cpu`).
    Cpu,
    /// A runner was up or starting for it, waiting for GitHub to hand it a job (`Waiting::RunnerStarting`).
    Booting,
    /// Its profile rebuilt its snapshot, pulled its image, or emptied what a host crash may have torn
    /// (`Waiting::Reseeding`, `Waiting::PullingImage`, `Waiting::Torn`).
    Preparing,
    /// Its repository was paused (`Waiting::Paused`).
    Paused,
    /// The host ran on its battery (`Waiting::OnBattery`).
    Battery,
    /// Runners of its repository failed, so none started for a while (`Waiting::Backoff`).
    Backoff,
    /// No pool served its labels (`Waiting::NoPool`).
    NoPool,
    /// A reason this build does not know: a `Waiting` from a newer daemon, or a cause a later runnerd stored.
    #[serde(other)]
    Other,
}

impl WaitCause {
    /// Every cause, in the order a split lists them when two took as long.
    pub const ALL: [WaitCause; 13] = [
        WaitCause::Barrier,
        WaitCause::Start,
        WaitCause::Slot,
        WaitCause::Budget,
        WaitCause::Host,
        WaitCause::Cpu,
        WaitCause::Booting,
        WaitCause::Preparing,
        WaitCause::Paused,
        WaitCause::Battery,
        WaitCause::Backoff,
        WaitCause::NoPool,
        WaitCause::Other,
    ];

    /// Its name as one word, the history's key and the API's: `barrier`, `start`, `slot`, `budget`, `host`, `cpu`,
    /// `booting`, `preparing`, `paused`, `battery`, `backoff`, `no-pool` or `other`.
    pub fn word(self) -> &'static str {
        match self {
            WaitCause::Barrier => "barrier",
            WaitCause::Start => "start",
            WaitCause::Slot => "slot",
            WaitCause::Budget => "budget",
            WaitCause::Host => "host",
            WaitCause::Cpu => "cpu",
            WaitCause::Booting => "booting",
            WaitCause::Preparing => "preparing",
            WaitCause::Paused => "paused",
            WaitCause::Battery => "battery",
            WaitCause::Backoff => "backoff",
            WaitCause::NoPool => "no-pool",
            WaitCause::Other => "other",
        }
    }

    /// The cause `word` names; [`WaitCause::Other`] for a word this build does not know.
    pub fn from_word(word: &str) -> WaitCause {
        WaitCause::ALL
            .into_iter()
            .find(|cause| cause.word() == word)
            .unwrap_or(WaitCause::Other)
    }

    /// What it is, as a label: `Behind a barrier`, `Host memory`.
    pub fn label(self) -> &'static str {
        match self {
            WaitCause::Barrier => "Behind a barrier",
            WaitCause::Start => "Behind an older job's start",
            WaitCause::Slot => "No free slot",
            WaitCause::Budget => "Memory budget",
            WaitCause::Host => "Host memory",
            WaitCause::Cpu => "CPU budget",
            WaitCause::Booting => "Runner starting",
            WaitCause::Preparing => "Snapshot, image or cleanup",
            WaitCause::Paused => "Repository paused",
            WaitCause::Battery => "Host on battery",
            WaitCause::Backoff => "Runners failing",
            WaitCause::NoPool => "No profile serves it",
            WaitCause::Other => "Another reason",
        }
    }
}

impl Waiting {
    /// What a wait for it is counted under in the history.
    pub fn cause(&self) -> WaitCause {
        match self {
            Waiting::BehindMemory { .. } | Waiting::BehindCpu { .. } => WaitCause::Barrier,
            Waiting::BehindStart { .. } => WaitCause::Start,
            Waiting::NoSlot { .. } => WaitCause::Slot,
            Waiting::Memory { .. } => WaitCause::Budget,
            Waiting::HostMemory { .. } => WaitCause::Host,
            Waiting::Cpu { .. } => WaitCause::Cpu,
            Waiting::RunnerStarting => WaitCause::Booting,
            Waiting::Reseeding { .. } | Waiting::PullingImage { .. } | Waiting::Torn { .. } => {
                WaitCause::Preparing
            }
            Waiting::Paused => WaitCause::Paused,
            Waiting::OnBattery => WaitCause::Battery,
            Waiting::Backoff { .. } => WaitCause::Backoff,
            Waiting::NoPool { .. } => WaitCause::NoPool,
            Waiting::Unknown => WaitCause::Other,
        }
    }
}

impl QueuedJob {
    /// Whether it waits for the host's memory: it does not fit what the host has to spare beside its other programs
    /// (`Waiting::HostMemory`), or it waits behind an older job for memory (`Waiting::BehindMemory`) and that job waits
    /// for the host's memory. Memory the host's other programs give back would let it start. `false` also where
    /// [`Status::host_wait`] cannot tell: a job not planned yet, one behind a job not in `status`, and a reason this
    /// client does not know.
    pub fn waits_for_host_memory(&self, status: &Status) -> bool {
        self.host_chain(status) == Some(true)
    }

    /// Whether it waits for the host's memory, or `None` when `status` cannot tell: no reason yet, a reason this
    /// client does not know, or a wait behind a job `status` does not show.
    fn host_chain(&self, status: &Status) -> Option<bool> {
        let mut job = self;
        // Each step goes to an older job, so a chain ends within the queue; the bound only guards a malformed one.
        for _ in 0..=status.queued.len() {
            match job.waiting.as_ref()? {
                Waiting::HostMemory { .. } => return Some(true),
                Waiting::Unknown => return None,
                Waiting::BehindMemory { job: older } => {
                    job = status.queued.iter().find(|queued| queued.id == *older)?;
                }
                _ => return Some(false),
            }
        }
        None
    }
}

/// Whether queued jobs wait for the host's memory, as [`Status::host_wait`] answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWait {
    /// A queued job asked about waits for the host's memory.
    Waits,
    /// None does, and the status shows every job asked about with why it waits.
    Clear,
    /// The status cannot say that none does: a repository asked about is not listed now, or the daemon that sent it
    /// does not say whether it is; the daemon is stopping; or a job asked about has no reason this client reads.
    Unavailable,
    /// The repository asked about is not one the daemon serves, so the answer would say nothing.
    UnknownRepository,
}

impl HostWait {
    /// Why there is no answer, as a script's error says it; `None` for an answer. `repository` is the one asked about.
    pub fn trouble(self, repository: Option<&str>) -> Option<String> {
        match self {
            HostWait::Waits | HostWait::Clear => None,
            HostWait::UnknownRepository => Some(format!(
                "{}: not a repository runnerd serves",
                repository.unwrap_or_default()
            )),
            HostWait::Unavailable => Some(format!(
                "runnerd cannot say whether {} waits for the host's memory: its queue is not listed now, the daemon \
                 stops, or it is older than this runnerctl",
                repository.map_or_else(|| "a job".to_owned(), |name| format!("a job of {name}"))
            )),
        }
    }
}

impl Status {
    /// Whether a queued job, of `repository` (`owner/name`, in any case, as GitHub reads it) when given, waits for
    /// the host's memory ([`QueuedJob::waits_for_host_memory`]): what a local build asks before it takes memory CI
    /// could start with. A job seen waiting is an answer however the rest stands; `Clear` needs every repository
    /// asked about listed and every job of them read, so a daemon that just started, cannot list, stops, or predates
    /// [`QueueShown`] is `Unavailable`, never `Clear`.
    pub fn host_wait(&self, repository: Option<&str>) -> HostWait {
        let asked: Vec<&RepositoryStatus> = match repository {
            None => self.repositories.iter().collect(),
            Some(asked) => match self
                .repositories
                .iter()
                .find(|served| served.name.eq_ignore_ascii_case(asked))
            {
                Some(served) => vec![served],
                None => return HostWait::UnknownRepository,
            },
        };
        let chains: Vec<Option<bool>> = self
            .queued
            .iter()
            .filter(|job| {
                asked
                    .iter()
                    .any(|served| served.name.eq_ignore_ascii_case(&job.repository))
            })
            .map(|job| job.host_chain(self))
            .collect();
        if chains.contains(&Some(true)) {
            HostWait::Waits
        } else if self.shutting_down
            || chains.contains(&None)
            || asked
                .iter()
                .any(|served| served.queue != QueueShown::Listed)
        {
            HostWait::Unavailable
        } else {
            HostWait::Clear
        }
    }
}

#[cfg(test)]
mod tests;
