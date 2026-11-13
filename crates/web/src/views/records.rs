//! `/records`: the records of failed jobs, each removable, with the limits they are kept within; the rest is the
//! job page's, where a record's shell opens. A record outlives its job's history: one whose job the history no
//! longer holds says so, and has no job page to link to.

use maud::{Markup, html};
use std::collections::BTreeSet;

use runnerd_core::api::{Column, JobQuery, RecordFilter, Records, Section, Status, Tone};
use runnerd_core::client::{Client, ClientError};

use super::{Page, heading, page, profile_class, time_ago};
use crate::format;

/// Everything the records page shows.
#[derive(Debug, Clone)]
pub(crate) struct RecordsView {
    pub(crate) now: u64,
    pub(crate) status: Status,
    pub(crate) records: Records,
    /// The jobs the history holds with a kept record; `None` from a daemon older than the record filter, which
    /// cannot say whose history is gone.
    pub(crate) in_history: Option<BTreeSet<u64>>,
}

/// The most kept records' jobs read from the history: as many as a page holds.
const KEPT_READ: u32 = 500;

impl RecordsView {
    pub(crate) async fn load(daemon: &Client, now: u64) -> Result<RecordsView, ClientError> {
        let kept = daemon
            .jobs(&JobQuery {
                record: Some(RecordFilter::Kept),
                limit: Some(KEPT_READ),
                ..JobQuery::default()
            })
            .await?;
        let in_history = kept
            .understood()
            .then(|| kept.page.jobs.iter().map(|job| job.id).collect());
        Ok(RecordsView {
            now,
            status: daemon.get("/v1/status").await?,
            records: daemon.get("/v1/records").await?,
            in_history,
        })
    }
}

/// The whole page.
pub(crate) fn render(view: &RecordsView) -> Markup {
    let main = html! {
        (heading(runnerd_core::api::View::Jobs.title(), Some(Page::Records), None))
        (section(&view.status, &view.records, view.in_history.as_ref(), view.now))
    };
    page(Section::Records.title(), Some(Page::Records), None, &main)
}

/// The records of failed jobs, each removable, and the limits they are kept within.
pub(crate) fn section(
    status: &Status,
    records: &Records,
    in_history: Option<&BTreeSet<u64>>,
    now: u64,
) -> Markup {
    html! {
        section #records .records aria-labelledby="records-title" {
            div.section-head {
                h2 #records-title .visually-hidden { (Section::Records.title()) }
                p class=(if records.summary_tone() == Tone::Warn { "muted over-limit" } else { "muted" }) { (format::sentence(&records.summary())) }
                @if !records.records.is_empty() {
                    button.button-quiet type="button"
                        data-on:click="confirm('Remove every record? Their shells can no longer be opened.') && @delete('/v1/records?all=true')"
                        { "Remove all" }
                }
            }
            @if records.records.is_empty() {
                p.muted { (format::sentence(Records::empty_reason())) "." }
            } @else {
                div.table-wrap {
                    table.job-table.record-table {
                        thead {
                            tr {
                                th scope="col" { (Column::Job.title()) }
                                th scope="col" { (Column::Made.title()) }
                                th scope="col" .number { (Column::Size.title()) }
                                th scope="col" { span.visually-hidden { "Remove" } }
                            }
                        }
                        tbody {
                            @for record in records.records.iter().rev() {
                                @let gone = in_history.is_some_and(|jobs| !jobs.contains(&record.job));
                                @let name = html! {
                                    span.swatch aria-hidden="true" {}
                                    span.job-name { (record.title().name) }
                                    span.repo {
                                        // A narrow screen breaks it after the owner, not inside a name.
                                        @if let Some((owner, name)) = record.repository.split_once('/') { (owner) "/" wbr; (name) } @else { (record.repository) }
                                        @if let Some(workflow) = &record.workflow { " · " (workflow) } }
                                    @if gone { span.history-gone { (format::sentence(Records::history_gone())) } }
                                };
                                tr class=(profile_class(status, &record.profile)) data-row {
                                    th scope="row" {
                                        @if gone { span.record-job { (name) } } @else { a href=(format!("/jobs/{}", record.job)) { (name) } }
                                    }
                                    td {
                                        (time_ago(record.created_at, now))
                                        @if records.shell_open(record.job) { span.muted { ", shell open" } }
                                    }
                                    td.number { (format::bytes(record.bytes)) }
                                    td.number {
                                        button.button-quiet type="button"
                                            data-on:click=(format!("confirm('Remove the record of job {}?') && @delete('/v1/records/{}')", record.job, record.job))
                                            aria-label=(format!("Remove the record of job {}", record.job)) { "Remove" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
