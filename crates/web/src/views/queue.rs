//! `/queue`: every job the daemon's status lists queued, oldest first, each with how long it has waited and why, as
//! core words it, and the repositories whose queued jobs the status leaves out, each with why (ADR 0174). The
//! overview shows the oldest few of the same list and links here. It reads `Status::queued` and nothing per row: the
//! one other read is which of the jobs the history holds, so a row opens the job's page only where there is one. It
//! never lists the history's queued jobs as waiting: the history keeps a job queued that the status leaves out.

use maud::{Markup, html};
use runnerd_core::api::{HealthKind, Status, View};
use runnerd_core::client::{Client, ClientError};

use super::local_pages::LocalPages;
use super::overview::queued_row;
use super::{Here, Live, Page, Parts, digest, heading, page, tone_class};

/// What the page shows.
#[derive(Debug, Clone)]
pub(crate) struct QueueView {
    pub(crate) now: u64,
    pub(crate) status: Status,
    /// The queued jobs the history holds, which have a page here.
    pub(crate) local: LocalPages,
}

impl QueueView {
    pub(crate) async fn load(daemon: &Client, now: u64) -> Result<QueueView, ClientError> {
        let status = daemon.get("/v1/status").await?;
        let local = LocalPages::read(daemon, &status).await?;
        Ok(QueueView { now, status, local })
    }
}

/// The live regions, by id, in the order the page has them, with their parts.
pub(crate) fn regions(view: &QueueView) -> [(&'static str, Markup, Parts); 2] {
    let count = Parts::default();
    let count_markup = count_of(view);
    let parts = Parts::default();
    let markup = waiting(view, &parts);
    [
        ("queue-count", count_markup, count),
        ("waiting", markup, parts),
    ]
}

/// The whole page.
pub(crate) fn render(view: &QueueView, here: Here<'_>) -> Markup {
    let regions = regions(view);
    let live = Live {
        url: "/live/queue",
        at: view.now,
        log: None,
        regions: regions
            .iter()
            .map(|(id, markup, _)| (*id, digest(&markup.0)))
            .collect(),
    };
    let [count, waiting] = regions;
    let main = html! {
        (heading("Waiting", Some(Page::Queue), Some(count.1)))
        p.queue-back { a href=(Page::Overview.path()) { "Back to the " (View::Overview.title().to_lowercase()) } }
        (waiting.1)
    };
    page("Waiting", here, Some(Page::Queue), Some(&live), &main)
}

/// How many wait and the oldest's wait, as the overview's health figure says it.
fn count_of(view: &QueueView) -> Markup {
    let figures = view.status.health(view.now).figures(None);
    let figure = figures
        .iter()
        .find(|figure| figure.kind == HealthKind::Waiting);
    html! {
        p #queue-count .queue-count {
            @if let Some(figure) = figure {
                span class=(tone_class(figure.tone)) {
                    span.queue-count-value { (figure.value) }
                    " " (figure.label.to_lowercase())
                    @for note in &figure.notes { span.muted { " · " (note) } }
                }
            }
        }
    }
}

/// The repositories left out, then every queued job.
fn waiting(view: &QueueView, parts: &Parts) -> Markup {
    let status = &view.status;
    let unlisted = status.unlisted();
    html! {
        section #waiting .queue.queue-page aria-label="Every waiting job" {
            @if !unlisted.is_empty() {
                ul.unlisted aria-label="Repositories not listed" {
                    @for left in &unlisted {
                        li.notice.tone-warn {
                            (left.line) " "
                            a href=(format!("/history?{}", serde_urlencoded::to_string([("repository", left.repository), ("outcome", "queued")]).unwrap_or_default())) {
                                "Its jobs queued in the history"
                            }
                        }
                    }
                }
            }
            @if status.queued.is_empty() {
                p.muted { (status.empty_queue_line()) }
            } @else {
                ol.queue-list {
                    @for job in &status.queued {
                        (queued_row(status, job, view.now, &view.local, parts))
                    }
                }
            }
        }
    }
}
