//! What `GET /v1/events` streams: one enum of everything that happens, wrapped with its ID and time. Durable events
//! are stored and get an ID, so a client resumes after them with `Last-Event-ID`; live events (samples and log
//! lines) are only sent to connected clients, and a client that missed them reads the job's samples or log instead.

use serde::{Deserialize, Serialize};

use super::jobs::{HostSample, JobSummary, Sample, Step};
use super::{Budget, SlotStatus};

/// One event as `GET /v1/events` sends it: the SSE `data` field. The SSE `event` field is `event.kind()` and the
/// SSE `id` field is `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Increases with every durable event; `None` for live events.
    pub id: Option<u64>,
    /// When it happened.
    pub at: u64,
    /// What happened.
    #[serde(flatten)]
    pub event: Event,
}

/// Everything that happens, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// A job that runners of this daemon may run was queued.
    #[serde(rename = "job.queued")]
    JobQueued {
        /// The job.
        job: JobSummary,
    },
    /// A runner of this daemon took the job.
    #[serde(rename = "job.started")]
    JobStarted {
        /// The job.
        job: JobSummary,
    },
    /// A step of a running job started or completed.
    #[serde(rename = "job.step")]
    JobStep {
        /// The job's ID.
        job: u64,
        /// The step as it is now.
        step: Step,
    },
    /// The job's record is final: it completed and its runner was removed, it was lost, or it ran elsewhere.
    #[serde(rename = "job.finished")]
    JobFinished {
        /// The job.
        job: JobSummary,
    },
    /// A slot's runner started, took a job, or was removed.
    #[serde(rename = "slot.changed")]
    SlotChanged {
        /// The profile.
        profile: String,
        /// The slot as it is now.
        slot: SlotStatus,
    },
    /// The memory caps in use changed.
    #[serde(rename = "budget.changed")]
    BudgetChanged {
        /// The budget as it is now.
        budget: Budget,
    },
    /// A profile's snapshot check finished.
    #[serde(rename = "cache.reseeded")]
    CacheReseeded {
        /// The profile.
        profile: String,
        /// What the seed container printed, such as why it did not reseed.
        output: String,
        /// Set when reseeding failed.
        error: Option<String>,
    },
    /// Live: a running job's container was sampled.
    #[serde(rename = "job.sample")]
    JobSample {
        /// The job's ID.
        job: u64,
        /// The sample.
        sample: Sample,
    },
    /// Live: the slice runners run in was sampled.
    #[serde(rename = "host.sample")]
    HostSample {
        /// The sample.
        sample: HostSample,
    },
    /// Live: a line of a running job's log was stored. Only sent with `logs=true`.
    #[serde(rename = "log.line")]
    LogLine {
        /// The job's ID.
        job: u64,
        /// The line's number in the job's log, 0-based.
        line: u64,
        /// The line, without its newline.
        text: String,
    },
}

impl Event {
    /// The `type` tag, also the SSE event name.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::JobQueued { .. } => "job.queued",
            Event::JobStarted { .. } => "job.started",
            Event::JobStep { .. } => "job.step",
            Event::JobFinished { .. } => "job.finished",
            Event::SlotChanged { .. } => "slot.changed",
            Event::BudgetChanged { .. } => "budget.changed",
            Event::CacheReseeded { .. } => "cache.reseeded",
            Event::JobSample { .. } => "job.sample",
            Event::HostSample { .. } => "host.sample",
            Event::LogLine { .. } => "log.line",
        }
    }

    /// Whether it is stored and can be resumed after; live events are not.
    pub fn is_durable(&self) -> bool {
        !matches!(
            self,
            Event::JobSample { .. } | Event::HostSample { .. } | Event::LogLine { .. }
        )
    }

    /// The job it is about, if any.
    pub fn job(&self) -> Option<u64> {
        match self {
            Event::JobQueued { job } | Event::JobStarted { job } | Event::JobFinished { job } => {
                Some(job.id)
            }
            Event::JobStep { job, .. }
            | Event::JobSample { job, .. }
            | Event::LogLine { job, .. } => Some(*job),
            Event::SlotChanged { .. }
            | Event::BudgetChanged { .. }
            | Event::CacheReseeded { .. }
            | Event::HostSample { .. } => None,
        }
    }
}

/// `GET /v1/events` query parameters. Resume with the `Last-Event-ID` header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventQuery {
    /// Only events about this job.
    pub job: Option<u64>,
    /// Also send `log.line` events.
    #[serde(default)]
    pub logs: bool,
}

impl EventQuery {
    /// Whether an event passes the filter.
    pub fn matches(&self, event: &Event) -> bool {
        (self.logs || !matches!(event, Event::LogLine { .. }))
            && self.job.is_none_or(|job| event.job() == Some(job))
    }
}
