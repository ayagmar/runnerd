//! `/caches`: every cache volume as a lane, its size, filling it against its bound where it has one, each one's size
//! over the last 30 days, its action, the shared build cache's week as figures, and what the action archives hold.

use maud::{Markup, html};
use runnerd_core::api::Machine;
use runnerd_core::api::{
    ActionArchives, BUILD_CACHE_TOOLS, BuildCacheStats, CacheHistory, CacheKind, CacheVolume,
    Caches, Section, Status,
};
use runnerd_core::client::{Client, ClientError};

use super::insights::figure_list;
use super::{Page, Parts, heading, page, profile_class, time_ago};
use crate::chart::{Base, Limit, Series, TimeChart, nice_top, ratio};
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

/// Its action, a labelled button: reseed for a snapshot, empty for the rest. The daemon refuses either while a
/// runner uses the volume.
fn action(volume: &CacheVolume) -> Markup {
    if let CacheKind::Snapshot { profile } = &volume.kind {
        html! {
            button.button-quiet type="button"
                data-on:click=(format!("confirm('Reseed {profile}\\'s snapshot from slot 1? Slot 1\\'s changes become the snapshot, and every slot\\'s own changes are dropped.') && @post('/v1/profiles/{profile}/reseed')"))
                aria-label=(format!("Reseed {profile}'s snapshot from slot 1")) { "Reseed from slot 1" }
        }
    } else {
        html! {
            button.button-quiet.empty-volume type="button"
                data-on:click=(format!("confirm('Empty {}? Its next job starts cold.') && @post('/v1/caches/{}/clean')", volume.name, volume.name))
                aria-label=(format!("Empty {}", volume.name)) { "Empty" }
        }
    }
}

/// The whole page.
pub(crate) fn render(view: &CachesView, machine: Option<&Machine>) -> Markup {
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
        @if let Some(archives) = &view.caches.actions {
            (action_archives(archives, view.now))
        }
    };
    page(
        Section::Caches.title(),
        machine,
        Some(Page::Caches),
        None,
        &main,
    )
}

/// Every volume as a lane: what it is, its size, as a fill against its bound when it has one, its size over 30 days
/// against its bound, and its action; the shared build cache's week as figures.
pub(crate) fn volumes(view: &CachesView) -> Markup {
    let start = view.now.saturating_sub(WINDOW);
    html! {
        div #volumes {
            @if view.caches.volumes.is_empty() {
                p.empty { "No volumes yet: runnerd creates them when a profile's first runner starts." }
            } @else {
                ol.volumes aria-label="Volumes" {
                    @for volume in &view.caches.volumes {
                        @let limit = volume.limit_status();
                        @let over = limit.as_ref().is_some_and(|limit| limit.over);
                        @let class = volume.kind.profile().map_or("p-none".to_owned(), |profile| profile_class(&view.status, profile));
                        @let points: Vec<(u64, u64)> = view.history.sizes.iter().filter(|size| size.name == volume.name).map(|size| (size.at, size.bytes)).collect();
                        @let peak = points.iter().map(|point| point.1).max().unwrap_or(volume.bytes).max(volume.bytes).max(volume.limit.unwrap_or(0));
                        li class=(format!("volume {class}{}", if limit.is_none() { " unbound" } else { "" })) {
                            div.volume-name {
                                span.swatch aria-hidden="true" {}
                                span {
                                    strong { (volume.kind.description()) }
                                    " " code { (volume.name) }
                                }
                            }
                            div.volume-size {
                                strong class=[over.then_some("over-limit")] { (format::bytes(volume.bytes)) }
                                span.muted {
                                    @if let Some(limit) = &limit { " of " (format::bytes(limit.limit)) ", " (limit.label) } @else { " no bound" }
                                }
                            }
                            // A volume with a bound fills its own track, as far as its bound or past it. One without
                            // has nothing to fill: on a shared scale a small volume beside a large one would read as
                            // empty, so its size is the figure above and its trend the chart beside it.
                            @if let Some(limit) = &limit {
                                @let scale = limit.limit.max(volume.bytes).max(1);
                                @let at = |bytes: u64| format!("{:.2}%", ratio(bytes, scale) * 100.0);
                                div.volume-fill role="img" aria-label=(format!("{} of {}, {}", format::bytes(volume.bytes), format::bytes(limit.limit), limit.label)) {
                                    span.volume-bound style=(format!("width: {}", at(limit.limit))) {}
                                    span.volume-used style=(format!("width: {}", at(volume.bytes))) {}
                                }
                            }
                            div.volume-trend {
                                @if points.len() > 1 {
                                    (TimeChart {
                                        title: format!("{} over 30 days", volume.kind.description()),
                                        start,
                                        end: view.now,
                                        window: None,
                                        live: None,
                                        series: vec![Series {
                                            label: String::new(),
                                            class: "memory",
                                            points,
                                            area: true,
                                        }],
                                        limits: limit.iter().map(|limit| Limit { value: limit.limit, label: String::new(), class: "" }).collect(),
                                        value: format::bytes,
                                        time: format::short_date,
                                        gap: WINDOW,
                                    }.render(nice_top(peak, Base::Binary), &Parts::default()))
                                } @else {
                                    span.muted { "One reading so far" }
                                }
                            }
                            div.volume-action { (action(volume)) }
                            @if volume.kind == CacheKind::Build && let Some(build) = &view.caches.build {
                                (build_cache(build))
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The shared build cache's last 7 days as figures, with what it is for and how GitHub Actions' cache goes through it.
fn build_cache(build: &BuildCacheStats) -> Markup {
    html! {
        div.build-cache {
            (figure_list("build-figures", &build.figures()))
            p.muted { "For " (BUILD_CACHE_TOOLS) ". " (format::sentence(&build.actions_cache_line())) "." }
        }
    }
}

/// The action archives runners read: what they hold, and each failure in its tone.
fn action_archives(archives: &ActionArchives, now: u64) -> Markup {
    html! {
        section.action-archives aria-label="Action archives" {
            h2 { "Action archives" }
            p.muted { (format::sentence(&archives.line(now))) "." }
            @if !archives.failures.is_empty() {
                ul.archive-failures class=(super::tone_class(archives.tone())) {
                    @for failure in &archives.failures {
                        li { (failure.line()) }
                    }
                }
            }
        }
    }
}
