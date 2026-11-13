//! `/caches`: every cache volume with its size against its bound, and each one's size over the last 30 days.

use maud::{Markup, html};
use runnerd_core::api::{
    BUILD_CACHE_TOOLS, CacheHistory, CacheKind, CacheVolume, Caches, Section, Status,
};
use runnerd_core::client::{Client, ClientError};

use super::{Page, Parts, heading, page, profile_class, time_ago};
use crate::chart::Sparkline;
use crate::format;

const WINDOW: u64 = 30 * 86_400;

/// Everything the caches page shows.
#[derive(Debug, Clone)]
pub(crate) struct CachesView {
    pub(crate) now: u64,
    pub(crate) status: Status,
    pub(crate) caches: Caches,
    pub(crate) history: CacheHistory,
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
        })
    }
}

/// Reseed for a snapshot, empty for the rest, folded under a quiet toggle until chosen; the daemon refuses either
/// while a runner uses the volume.
fn action(volume: &CacheVolume) -> Markup {
    let button = if let CacheKind::Snapshot { profile } = &volume.kind {
        html! {
            button.button-quiet type="button"
                data-on:click=(format!("@post('/v1/profiles/{profile}/reseed')"))
                aria-label=(format!("Reseed {profile}'s snapshot from slot 1")) { "Reseed from slot 1" }
        }
    } else {
        html! {
            button.button-danger type="button"
                data-on:click=(format!("confirm('Empty {}? Its next job starts cold.') && @post('/v1/caches/{}/clean')", volume.name, volume.name))
                aria-label=(format!("Empty {}", volume.name)) { "Empty it" }
        }
    };
    html! {
        details.volume-action {
            summary aria-label=(format!("Actions for {}", volume.name)) { "…" }
            div.volume-action-menu { (button) }
        }
    }
}

/// The whole page.
pub(crate) fn render(view: &CachesView) -> Markup {
    let total: u64 = view.caches.volumes.iter().map(|volume| volume.bytes).sum();
    let figure = html! {
        p.page-figure title="Docker measures volumes slowly, so runnerd does at start and whenever no runner runs after jobs ended." {
            strong { (format::bytes(total)) }
            span.muted {
                " in " (view.caches.volumes.len()) " volumes"
                @if let Some(at) = view.caches.measured_at { ", measured " (time_ago(at, view.now)) }
                @else { ", not measured yet" }
            }
        }
    };
    let main = html! {
        (heading(runnerd_core::api::View::Capacity.title(), Some(Page::Caches), Some(figure)))
        (volumes(view))
    };
    page(Section::Caches.title(), Some(Page::Caches), None, &main)
}

/// One table of every volume: what it caches, its size against its bound, its size over 30 days as a sparkline, and
/// its action folded away until chosen.
pub(crate) fn volumes(view: &CachesView) -> Markup {
    let start = view.now.saturating_sub(WINDOW);
    html! {
        div #volumes {
            @if view.caches.volumes.is_empty() {
                p.muted { "No volumes yet: runnerd creates them when a profile's first runner starts." }
            } @else {
                div.table-wrap {
                    table.job-table.cache-table {
                        thead {
                            tr {
                                th scope="col" { "Volume" }
                                th scope="col" .number { "Size" }
                                th scope="col" { "Bound" }
                                th scope="col" { "30 days" }
                                th scope="col" { span.visually-hidden { "Action" } }
                            }
                        }
                        tbody {
                            @for volume in &view.caches.volumes {
                                @let limit = volume.limit_status();
                                @let class = volume.kind.profile().map_or("p-none".to_owned(), |profile| profile_class(&view.status, profile));
                                @let points: Vec<(u64, u64)> = view.history.sizes.iter().filter(|size| size.name == volume.name).map(|size| (size.at, size.bytes)).collect();
                                @let peak = points.iter().map(|point| point.1).max().unwrap_or(volume.bytes).max(volume.bytes).max(volume.limit.unwrap_or(0));
                                tr class=(class) data-row {
                                    th scope="row" {
                                        span.record-job {
                                            span.swatch aria-hidden="true" {}
                                            span.job-name { (volume.kind.description()) }
                                            span.repo { code { (volume.name) } }
                                            @if volume.kind == CacheKind::Build && let Some(build) = &view.caches.build {
                                                span.repo.cache-note { "For " (BUILD_CACHE_TOOLS) ". " (format::sentence(&build.week_line())) ". " (format::sentence(&build.actions_cache_line())) }
                                            }
                                        }
                                    }
                                    td.number data-label="size" {
                                        span class=[limit.as_ref().filter(|limit| limit.over).map(|_| "over-limit")] { (format::bytes(volume.bytes)) }
                                    }
                                    td.muted data-label="bound" {
                                        @if let Some(limit) = &limit { (format::bytes(limit.limit)) ", " (limit.label) } @else { "–" }
                                    }
                                    td.spark {
                                        @if points.len() > 1 {
                                            (Sparkline {
                                                points: &points,
                                                start,
                                                end: view.now,
                                                window: WINDOW,
                                                top: peak,
                                                class: "memory",
                                                live: None,
                                            }.render(&Parts::default()))
                                        } @else {
                                            span.muted { "one reading" }
                                        }
                                    }
                                    td.action { (action(volume)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
