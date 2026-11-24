//! Which queued jobs have a page on this dashboard: the IDs of the history's queued jobs as last read, or nothing
//! known while that read fails. It owns only that answer and when it is worth asking again; `overview::queued_row`
//! decides what a row links to from it. Nothing survives a reload of the page or a restart: each page reads it anew.

use std::collections::BTreeSet;

use runnerd_core::api::{JobPage, JobQuery, JobState, Status};
use runnerd_core::client::{Client, ClientError};

/// The most jobs `GET /v1/jobs` lists at once.
const MAX_JOBS: u32 = 500;

/// The queued jobs the history holds, or that the read of them failed (ADR 0174).
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalPages(Known);

#[derive(Debug, Clone, Default)]
enum Known {
    /// The read failed: every row opens GitHub, and the read is tried again with the next status.
    #[default]
    Failed,
    /// Read for the queued jobs of a status: `asked`, its queued IDs then, and `local`, those of the history's queued
    /// jobs (newest 500). A status that lists none is not read, and asks about none.
    Read {
        asked: BTreeSet<u64>,
        local: BTreeSet<u64>,
    },
}

impl LocalPages {
    /// Reads them from the history for `status`'s queued jobs, unless it lists none, which need no link.
    pub(crate) async fn read(daemon: &Client, status: &Status) -> LocalPages {
        let asked: BTreeSet<u64> = status.queued.iter().map(|job| job.id).collect();
        if asked.is_empty() {
            return LocalPages(Known::Read {
                asked,
                local: BTreeSet::new(),
            });
        }
        let page: Result<JobPage, ClientError> = daemon
            .get_query(
                "/v1/jobs",
                &JobQuery {
                    state: Some(JobState::Queued),
                    limit: Some(MAX_JOBS),
                    ..JobQuery::default()
                },
            )
            .await;
        LocalPages(page.map_or(Known::Failed, |page| Known::Read {
            asked,
            local: page.jobs.iter().map(|job| job.id).collect(),
        }))
    }

    /// Whether job `id` has a page here, as far as is known.
    pub(crate) fn has(&self, id: u64) -> bool {
        matches!(&self.0, Known::Read { local, .. } if local.contains(&id))
    }

    /// Whether `status` needs them read again: the last read failed, or `status` lists a queued job it did not ask
    /// about, such as one a repository's listing brings back with no event of its own.
    pub(crate) fn stale(&self, status: &Status) -> bool {
        match &self.0 {
            Known::Failed => true,
            Known::Read { asked, .. } => status.queued.iter().any(|job| !asked.contains(&job.id)),
        }
    }
}
