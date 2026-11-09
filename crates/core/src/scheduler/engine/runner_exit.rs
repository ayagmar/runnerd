//! What a runner's end says: whether it ran a job, whether that job ended, and whether a job the last poll listed
//! queued may be gone with it. `Exit` is worked out once at each place a runner goes, and the engine acts on its
//! answers. It must not touch the engine, and holds nothing past the release that makes it.

use std::time::SystemTime;

use crate::scheduler::IDLE_TIMEOUT;

/// What a runner logs as it takes a job.
pub(super) const TOOK_JOB: &str = "Running job: ";

/// What a runner logs once GitHub gave it a session: it started and connected, whatever ends it after that.
const CONNECTED: &str = "Listening for Jobs";

/// What a runner's log says of the job it may have taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Logged {
    /// It took a job.
    TookJob,
    /// It took none: it `connected` to GitHub and left, or never did.
    NoJob { connected: bool },
    /// Docker gave no log, so nothing is known: it may have run a job, and it says nothing of its pool's next start.
    Unread,
}

impl Logged {
    /// Reads a runner's `logs`, if Docker gave them. One that waited `IDLE_TIMEOUT` since `started_at` connected too,
    /// unless they no longer show it.
    pub(super) fn read(logs: Option<&str>, started_at: SystemTime) -> Self {
        let Some(logs) = logs else {
            return Self::Unread;
        };
        if logs.contains(TOOK_JOB) {
            return Self::TookJob;
        }
        Self::NoJob {
            connected: logs.contains(CONNECTED)
                || started_at
                    .elapsed()
                    .is_ok_and(|waited| waited >= IDLE_TIMEOUT),
        }
    }
}

/// How a runner went, with what each way knows of it. Computed once where it goes, from nothing a restart or a reload
/// keeps: nothing of it survives either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Exit {
    /// It ended on its own and its logs were read for. `job` is what its capture named by then.
    Released {
        logged: Logged,
        listed_busy: bool,
        job: Option<u64>,
    },
    /// The engine removed it, busy or not, and with it any job it ran.
    Killed { busy: bool },
    /// The engine removed it as idle, GitHub having agreed. `busy` is an adopted runner no listing has yet shown idle.
    RemovedIdle { busy: bool },
}

impl Exit {
    /// Whether it may have run a job, so that what it leaves behind is checked. Unread logs: it may have run one.
    pub(super) fn ran(&self) -> bool {
        match *self {
            Self::Released {
                logged,
                listed_busy,
                job,
            } => listed_busy || job.is_some() || !matches!(logged, Logged::NoJob { .. }),
            Self::Killed { busy } | Self::RemovedIdle { busy } => busy,
        }
    }

    /// Whether a job of its repository ended with it, which keeps that repository active for the next push (ADR
    /// 0107). A runner removed as idle had no job to end, whatever a listing last said.
    pub(super) fn job_ended(&self) -> bool {
        match *self {
            Self::Released {
                logged,
                listed_busy,
                job,
            } => listed_busy || job.is_some() || logged == Logged::TookJob,
            Self::Killed { busy } => busy,
            Self::RemovedIdle { .. } => false,
        }
    }

    /// Whether a job the last poll listed queued may be the one it ran, or if it connected and took none, the one
    /// cancelled under it: with no hold, a pass would start a runner for that job from the listing (ADR 0104). A
    /// runner the engine removed spends none.
    pub(super) fn spends(&self) -> bool {
        match *self {
            Self::Released { logged, job, .. } => {
                job.is_none() && logged != (Logged::NoJob { connected: false })
            }
            Self::Killed { .. } | Self::RemovedIdle { .. } => false,
        }
    }
}
