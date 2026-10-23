//! `/caches`: every cache volume with its size against its bound, and each one's size over the last 30 days.

use maud::{Markup, html};
use runnerd_core::api::{CacheHistory, CacheKind, CacheVolume, Caches, Status};

use super::{Page, Parts, page, profile_class, time_ago};
use crate::chart::{Base, Series, TimeChart, nice_top};
use crate::daemon::{Daemon, Error};
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
    pub(crate) async fn load(daemon: &Daemon, now: u64) -> Result<CachesView, Error> {
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
                    @if points.len() > 1 {
                        (TimeChart {
                            title: format!("Size of {} over 30 days", volume.name),
                            start,
                            end: view.now,
                            window: None,
                            live: None,
                            series: vec![Series { label: "size".to_owned(), class: "memory", points, area: true }],
                            limit: volume.limit.map(|limit| (limit, format!("emptied above {}", format::bytes(limit)))),
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
