//! `/caches`: every cache volume with its size against its bound, and each one's size over the last 30 days; then
//! the records of failed jobs, the other disk runnerd keeps, with their limits.

use maud::{Markup, html};
use runnerd_core::api::{CacheHistory, CacheKind, CacheVolume, Caches, Records, Status, Tone};
use runnerd_core::client::{Client, ClientError};

use super::{Page, Parts, page, profile_class, time_ago};
use crate::chart::{Base, Limit, Series, TimeChart, nice_top};
use crate::format;

const WINDOW: u64 = 30 * 86_400;

/// Everything the caches page shows.
#[derive(Debug, Clone)]
pub(crate) struct CachesView {
    pub(crate) now: u64,
    pub(crate) status: Status,
    pub(crate) caches: Caches,
    pub(crate) history: CacheHistory,
    pub(crate) records: Records,
}

impl CachesView {
    pub(crate) async fn load(daemon: &Client, now: u64) -> Result<CachesView, ClientError> {
        Ok(CachesView {
            now,
            status: daemon.get("/v1/status").await?,
            caches: daemon.get("/v1/caches").await?,
            history: daemon
                .get(&format!(
                    "/v1/caches/history?since={}",
                    now.saturating_sub(WINDOW)
                ))
                .await?,
            records: daemon.get("/v1/records").await?,
        })
    }
}

/// Reseed for a snapshot, empty for the rest; the daemon refuses either while a runner uses the volume.
fn action(volume: &CacheVolume) -> Markup {
    if let CacheKind::Snapshot { profile } = &volume.kind {
        html! {
            button.button-quiet.volume-action type="button"
                data-on:click=(format!("@post('/v1/profiles/{profile}/reseed')"))
                aria-label=(format!("Reseed {profile}'s snapshot from slot 1")) { "Reseed" }
        }
    } else {
        html! {
            button.button-quiet.volume-action type="button"
                data-on:click=(format!("confirm('Empty {}? Its next job starts cold.') && @post('/v1/caches/{}/clean')", volume.name, volume.name))
                aria-label=(format!("Empty {}", volume.name)) { "Empty" }
        }
    }
}

/// The whole page.
pub(crate) fn render(view: &CachesView) -> Markup {
    let total: u64 = view.caches.volumes.iter().map(|volume| volume.bytes).sum();
    let main = html! {
        div.page-head {
            h1.visually-hidden { "Caches" }
            p.figures title="Docker measures volumes slowly, so runnerd does at start and whenever no runner runs after jobs ended." {
                span.big { (format::bytes(total)) }
                span.muted {
                    " in " (view.caches.volumes.len()) " volumes"
                    @if let Some(at) = view.caches.measured_at { ", measured " (time_ago(at, view.now)) }
                    @else { ", not measured yet" }
                }
            }
        }
        (volumes(view))
        (records(&view.status, &view.records, view.now))
    };
    page("Caches", Some(Page::Caches), None, &main)
}

/// A card per volume: its size, its bound, and its size over time.
pub(crate) fn volumes(view: &CachesView) -> Markup {
    let start = view.now.saturating_sub(WINDOW);
    html! {
        @if view.caches.volumes.is_empty() {
            p.muted { "No volumes yet: runnerd creates them when a profile's first runner starts." }
        }
        ul #volumes .volume-grid {
            @for volume in &view.caches.volumes {
                @let label = volume.kind.description();
                @let limit = volume.limit_status();
                @let class = volume.kind.profile().map_or("p-none".to_owned(), |profile| profile_class(&view.status, profile));
                @let points: Vec<(u64, u64)> = view.history.sizes.iter().filter(|size| size.name == volume.name).map(|size| (size.at, size.bytes)).collect();
                @let peak = points.iter().map(|point| point.1).max().unwrap_or(volume.bytes).max(volume.bytes);
                li class=(format!("volume {class}")) {
                    div.volume-head {
                        h2 { (label) }
                        (action(volume))
                    }
                    p.volume-name { code { (volume.name) } }
                    p.volume-size {
                        span class=(if limit.as_ref().is_some_and(|limit| limit.over) { "big over-limit" } else { "big" }) { (format::bytes(volume.bytes)) }
                        @if let Some(limit) = &limit { span.muted { " of " (format::bytes(limit.limit)) ", " (limit.label) } }
                    }
                    @if volume.kind == CacheKind::Build && let Some(build) = &view.caches.build {
                        p.muted { (build.week_line()) }
                        p.muted { (build.actions_cache_line()) }
                    }
                    @if points.len() > 1 {
                        (TimeChart {
                            title: format!("Size of {} over 30 days", volume.name),
                            start,
                            end: view.now,
                            window: None,
                            live: None,
                            series: vec![Series { label: "size".to_owned(), class: "memory", points, area: true }],
                            limits: limit.map(|limit| Limit { value: limit.limit, label: limit.label, class: "" }).into_iter().collect(),
                            value: format::bytes,
                            time: format::short_date,
                            gap: u64::MAX,
                        }.render(nice_top(peak.max(volume.limit.unwrap_or(0)), Base::Binary), &Parts::default()))
                    } @else {
                        p.muted { "One measurement so far." }
                    }
                }
            }
        }
    }
}

/// The records of failed jobs, each removable, and the limits they are kept within.
pub(crate) fn records(status: &Status, records: &Records, now: u64) -> Markup {
    html! {
        section #records .records aria-labelledby="records-title" {
            div.section-head {
                h2 #records-title { "Records of failed jobs" }
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
                                th scope="col" { "Job" }
                                th scope="col" { "Made" }
                                th scope="col" .number { "Size" }
                                th scope="col" { span.visually-hidden { "Remove" } }
                            }
                        }
                        tbody {
                            @for record in records.records.iter().rev() {
                                tr class=(profile_class(status, &record.profile)) {
                                    th scope="row" {
                                        a href=(format!("/jobs/{}", record.job)) {
                                            span.swatch aria-hidden="true" {}
                                            span.job-name { (record.title().name) }
                                            span.repo {
                                                // A narrow screen breaks it after the owner, not inside a name.
                                                @if let Some((owner, name)) = record.repository.split_once('/') { (owner) "/" wbr; (name) } @else { (record.repository) }
                                                @if let Some(workflow) = &record.workflow { " · " (workflow) } }
                                        }
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
