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
    /// The read failed: every row opens GitHub, and the read is tried again when the status is.
    #[default]
    Failed,
    /// The history's queued jobs, newest 500, as last read; none when the status listed nothing queued.
    Read(BTreeSet<u64>),
}

impl LocalPages {
    /// Reads them from the history, unless `status` lists no queued job, which needs no link.
    pub(crate) async fn read(daemon: &Client, status: &Status) -> LocalPages {
        if status.queued.is_empty() {
            return LocalPages(Known::Read(BTreeSet::new()));
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
        LocalPages(page.map_or(Known::Failed, |page| {
            Known::Read(page.jobs.iter().map(|job| job.id).collect())
        }))
    }

    /// Whether job `id` has a page here, as far as is known.
    pub(crate) fn has(&self, id: u64) -> bool {
        matches!(&self.0, Known::Read(ids) if ids.contains(&id))
    }

    /// Whether the last read failed, so a page asks again with the status.
    pub(crate) fn failed(&self) -> bool {
        matches!(self.0, Known::Failed)
    }
}
