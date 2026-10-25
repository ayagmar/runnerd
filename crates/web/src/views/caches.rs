//! `/caches`: every cache volume with its size against its bound, and each one's size over the last 30 days; then
//! the records of failed jobs, the other disk runnerd keeps, with their limits.

use maud::{Markup, html};
use runnerd_core::api::{
    BuildCacheStats, CacheHistory, CacheKind, CacheVolume, Caches, Records, Status,
};
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

/// What a volume is called, and the profile it belongs to; none for a shared one.
struct Kind<'volume> {
    label: String,
    profile: Option<&'volume str>,
}

fn kind(volume: &CacheVolume) -> Kind<'_> {
    let (label, profile) = match &volume.kind {
        CacheKind::Snapshot { profile } => (format!("{profile} snapshot"), Some(profile)),
        CacheKind::Slot { profile, slot } => {
            (format!("{profile} slot {slot} layer"), Some(profile))
        }
        CacheKind::Pnpm => ("pnpm store, shared".to_owned(), None),
        CacheKind::Maven => ("Maven repository, shared".to_owned(), None),
        CacheKind::Mirror => ("Docker Hub mirror".to_owned(), None),
        CacheKind::Build => ("Build cache, shared".to_owned(), None),
    };
    Kind {
        label,
        profile: profile.map(String::as_str),
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
                @let Kind { label, profile } = kind(volume);
                @let class = profile.map_or("p-none".to_owned(), |profile| profile_class(&view.status, profile));
                @let points: Vec<(u64, u64)> = view.history.sizes.iter().filter(|size| size.name == volume.name).map(|size| (size.at, size.bytes)).collect();
                @let peak = points.iter().map(|point| point.1).max().unwrap_or(volume.bytes).max(volume.bytes);
                li class=(format!("volume {class}")) {
                    div.volume-head {
                        h2 { (label) }
                        (action(volume))
                    }
                    p.volume-name { code { (volume.name) } }
                    p.volume-size {
                        span.big { (format::bytes(volume.bytes)) }
                        @if let Some(limit) = volume.limit { span.muted { " of " (format::bytes(limit)) } }
                    }
                    @if volume.kind == CacheKind::Build && let Some(build) = &view.caches.build {
                        (build_cache(build))
                    }
                    @if points.len() > 1 {
                        (TimeChart {
                            title: format!("Size of {} over 30 days", volume.name),
                            start,
                            end: view.now,
                            window: None,
                            live: None,
                            series: vec![Series { label: "size".to_owned(), class: "memory", points, area: true }],
                            limits: volume.limit.map(|limit| Limit { value: limit, label: limit_label(volume, limit), class: "" }).into_iter().collect(),
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

/// What the chart calls a volume's limit: the build cache is trimmed to it, the rest emptied above it.
fn limit_label(volume: &CacheVolume, limit: u64) -> String {
    if volume.kind == CacheKind::Build {
        format!("trimmed to {}", format::bytes(limit))
    } else {
        format!("emptied above {}", format::bytes(limit))
    }
}

/// The build cache's entries and what it served the last 7 days' jobs.
fn build_cache(build: &BuildCacheStats) -> Markup {
    let week = &build.week;
    html! {
        p.muted {
            (build.entries) " entries; "
            @match week.hit_percent() {
                Some(percent) => {
                    (percent) "% of " (week.lookups()) " lookups hit in 7 days, "
                    (format::bytes(week.read_bytes)) " served, " (format::bytes(week.written_bytes)) " stored"
                    @if week.refused > 0 { ", " (week.refused) " uploads refused" }
                }
                None => "no lookups in 7 days",
            }
        }
    }
}

/// The records of failed jobs, each removable, and the limits they are kept within.
pub(crate) fn records(status: &Status, records: &Records, now: u64) -> Markup {
    let limits = &records.limits;
    html! {
        section #records .records aria-labelledby="records-title" {
            div.section-head {
                h2 #records-title { "Records of failed jobs" }
                p.muted {
                    (records.records.len()) " of " (limits.max_count) ", "
                    (format::bytes(records.bytes())) " of " (format::bytes(limits.max_total))
                    ", each kept " (format::duration(limits.max_age))
                }
                @if !records.records.is_empty() {
                    button.button-quiet type="button"
                        data-on:click="confirm('Remove every record? Their shells can no longer be opened.') && @delete('/v1/records?all=true')"
                        { "Remove all" }
                }
            }
            @if records.records.is_empty() {
                p.muted {
                    "None kept. A profile with " code { "record_failures = true" }
                    " keeps each failed job's runner as a record, to open a shell where it failed."
                }
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
                                        @if records.shells.contains(&record.job) { span.muted { ", shell open" } }
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
