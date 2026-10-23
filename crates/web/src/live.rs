//! Live pages: one SSE response per open page that sends Datastar element patches. Each subscribes to the daemon's
//! `/v1/events`, keeps the page's data current from the events (reading the daemon again where an event only says
//! that something changed), and re-renders the regions that changed, at most every `SETTLE`. Only what the page
//! lacks is sent: a region whose HTML differs, or when only some of its parts (`views::Parts`) do, those parts. On
//! connecting, the stream skips the regions the page already has, which the page names by digest in its `shown`
//! signal; after each update the stream sets that signal to what the page shows now, and the time, which live
//! charts follow. The stream ends when the page closes, the daemon's stream ends, or the dashboard stops.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use datastar::consts::Namespace;
use datastar::prelude::{ElementPatchMode, PatchElements, PatchSignals};
use futures_util::{Stream, StreamExt, stream};
use maud::Markup;
use runnerd_core::api::{Event, EventQuery, Job};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::App;
use crate::daemon::Error;
use crate::views::job::{self, JobView};
use crate::views::{self, Part, Parts, Tone, digest, overview};

/// How long to collect events before rendering: a burst (a slot and the budget change together) renders once.
const SETTLE: Duration = Duration::from_millis(250);

type Patches = mpsc::Sender<SseEvent>;

fn respond(
    receiver: mpsc::Receiver<SseEvent>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let events = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|event| (Ok(event), receiver))
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

/// What a page says it shows when its stream connects: the `shown` signal, which Datastar sends as JSON in the
/// `datastar` query parameter.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Signals {
    datastar: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Page {
    shown: Shown,
}

/// The time a page was rendered at, and a digest of each region it had then, by id.
#[derive(Debug, Deserialize)]
struct Shown {
    at: u64,
    #[serde(flatten)]
    regions: HashMap<String, String>,
}

impl Signals {
    /// What the page shows; none from a page that sent no `shown` signal or one this dashboard did not write.
    fn shown(&self) -> Option<Shown> {
        let page: Page = serde_json::from_str(self.datastar.as_deref()?).ok()?;
        Some(page.shown)
    }
}

/// A region as rendered: its HTML, its parts, and the rest of it, which only a whole region's patch can change.
struct Rendered {
    html: String,
    frame: String,
    parts: Vec<Part>,
}

impl Rendered {
    fn new(markup: Markup, parts: Parts) -> Rendered {
        let html = markup.into_string();
        let (parts, clocked) = parts.into_parts();
        let mut frame = html.clone();
        // A clocked value with its quotes, so it is cut from the attribute it is and not from a longer one.
        let clocked = clocked.iter().map(|value| format!("\"{value}\""));
        for cut in parts.iter().map(|part| part.html.clone()).chain(clocked) {
            if let Some(at) = frame.find(&cut) {
                frame.replace_range(at..at + cut.len(), "");
            }
        }
        // Which fixed parts the region has is the frame's: one coming or going changes the region.
        for part in parts.iter().filter(|part| part.group.is_none()) {
            frame.push('\n');
            frame.push_str(&part.id);
        }
        Rendered { html, frame, parts }
    }
}

/// The page is gone.
struct Closed;

/// What a page has of each region, by id, so what is unchanged is not sent again.
#[derive(Default)]
struct Sent(HashMap<&'static str, Rendered>);

impl Sent {
    /// Takes the regions the page had when it was rendered at `shown.at`, rendered here at that time, as sent: those
    /// whose digests match the page's.
    fn seed(
        &mut self,
        regions: impl IntoIterator<Item = (&'static str, Markup, Parts)>,
        shown: &Shown,
    ) {
        for (id, markup, parts) in regions {
            let rendered = Rendered::new(markup, parts);
            if shown.regions.get(id) == Some(&digest(&rendered.html)) {
                self.0.insert(id, rendered);
            }
        }
    }

    /// Sends what of the region the page lacks; whether anything was sent.
    async fn region(
        &mut self,
        patches: &Patches,
        id: &'static str,
        rendered: Rendered,
    ) -> Result<bool, Closed> {
        let mut events = Vec::new();
        match self.0.get(id) {
            Some(sent) if sent.html == rendered.html => return Ok(false),
            Some(sent) if sent.frame == rendered.frame => {
                let before: HashMap<&str, &Part> = sent
                    .parts
                    .iter()
                    .map(|part| (part.id.as_str(), part))
                    .collect();
                let now: HashSet<&str> =
                    rendered.parts.iter().map(|part| part.id.as_str()).collect();
                let mut html = String::new();
                let mut svg = String::new();
                for part in &rendered.parts {
                    match (before.get(part.id.as_str()), &part.group) {
                        (Some(sent), _) if **sent == *part => {}
                        (None, Some(group)) => events.push(
                            PatchElements::new(part.html.clone())
                                .selector(format!("#{group}"))
                                .mode(ElementPatchMode::Append)
                                .namespace(Namespace::Svg),
                        ),
                        (_, None) => html.push_str(&part.html),
                        (_, Some(_)) => svg.push_str(&part.html),
                    }
                }
                if !html.is_empty() {
                    events.push(PatchElements::new(html));
                }
                if !svg.is_empty() {
                    events.push(PatchElements::new(svg).namespace(Namespace::Svg));
                }
                let gone: Vec<String> = sent
                    .parts
                    .iter()
                    .filter(|part| !now.contains(part.id.as_str()))
                    .map(|part| format!("#{}", part.id))
                    .collect();
                if !gone.is_empty() {
                    events.push(PatchElements::new_remove(gone.join(", ")));
                }
            }
            _ => events.push(PatchElements::new(rendered.html.clone())),
        }
        self.0.insert(id, rendered);
        for event in events {
            if patches.send(event.write_as_axum_sse_event()).await.is_err() {
                return Err(Closed);
            }
        }
        Ok(true)
    }

    /// Sends what the page lacks of each region, then the `shown` signal: the time, and the digest of each region
    /// that changed.
    async fn update(
        &mut self,
        patches: &Patches,
        at: u64,
        regions: impl IntoIterator<Item = (&'static str, Markup, Parts)>,
    ) -> Result<(), Closed> {
        let mut shown = serde_json::Map::new();
        shown.insert("at".to_owned(), at.into());
        for (id, markup, parts) in regions {
            if self
                .region(patches, id, Rendered::new(markup, parts))
                .await?
            {
                let digest = self.0.get(id).map(|sent| digest(&sent.html));
                shown.insert(id.to_owned(), digest.unwrap_or_default().into());
            }
        }
        let signals = serde_json::json!({ "shown": shown }).to_string();
        patches
            .send(PatchSignals::new(signals).write_as_axum_sse_event())
            .await
            .map_err(|_| Closed)
    }
}

async fn notice(patches: &Patches, error: &Error) {
    let event = PatchElements::new(views::notice(&error.to_string(), Tone::Failed).into_string())
        .write_as_axum_sse_event();
    let _ = patches.send(event).await;
}

/// Waits `SETTLE` for more events, adding them to `batch`; `false` once the daemon's stream ended.
async fn settle(
    events: &mut (impl Stream<Item = runnerd_core::api::EventEnvelope> + Unpin),
    batch: &mut Vec<Event>,
) -> bool {
    let deadline = tokio::time::sleep(SETTLE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return true,
            event = events.next() => match event {
                Some(envelope) => batch.push(envelope.event),
                None => return false,
            },
        }
    }
}

/// Waits for the next event; `None` when the stream, the page or the dashboard ended.
async fn next_event(
    events: &mut (impl Stream<Item = runnerd_core::api::EventEnvelope> + Unpin),
    patches: &Patches,
    stopping: &mut watch::Receiver<bool>,
) -> Option<Event> {
    tokio::select! {
        event = events.next() => event.map(|envelope| envelope.event),
        () = patches.closed() => None,
        _ = stopping.wait_for(|stopping| *stopping) => None,
    }
}

/// `GET /live/overview`.
pub(crate) async fn overview(
    State(app): State<App>,
    Query(signals): Query<Signals>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (patches, receiver) = mpsc::channel(16);
    tokio::spawn(async move {
        if let Err(error) = run_overview(&app, &patches, signals.shown()).await {
            debug!("overview stream ended: {error}");
            notice(&patches, &error).await;
        }
    });
    respond(receiver)
}

async fn run_overview(app: &App, patches: &Patches, shown: Option<Shown>) -> Result<(), Error> {
    // Subscribed before reading, so nothing that happens in between is missed.
    let events = app.daemon.events(&EventQuery::default()).await?;
    let mut events = Box::pin(events);
    let mut stopping = app.stopping.clone();
    let mut data = overview::Overview::load(&app.daemon, crate::now()).await?;
    let mut sent = Sent::default();
    if let Some(shown) = shown {
        data.now = shown.at;
        sent.seed(overview::regions(&data), &shown);
    }
    loop {
        data.now = crate::now();
        if sent
            .update(patches, data.now, overview::regions(&data))
            .await
            .is_err()
        {
            return Ok(());
        }
        let Some(first) = next_event(&mut events, patches, &mut stopping).await else {
            return Ok(());
        };
        let mut batch = vec![first];
        let open = settle(&mut events, &mut batch).await;
        let mut jobs_changed = false;
        for event in batch {
            match event {
                Event::JobSample { job, sample } => data.add_job_sample(job, sample),
                Event::HostSample { sample } => data.add_host_sample(sample),
                Event::LogLine { .. }
                | Event::CacheReseeded { .. }
                | Event::BudgetChanged { .. }
                | Event::ProfileChanged { .. }
                | Event::ProfileRemoved { .. }
                | Event::RepositoryChanged { .. }
                | Event::RepositoryRemoved { .. } => {}
                Event::JobQueued { .. }
                | Event::JobStarted { .. }
                | Event::JobStep { .. }
                | Event::JobFinished { .. }
                | Event::SlotChanged { .. } => jobs_changed = true,
            }
        }
        // The budget, profiles and repositories come with it; the status is cheap to read on every update.
        data.status = app.daemon.get("/v1/status").await?;
        if jobs_changed {
            data.reload_running(&app.daemon).await?;
            data.recent = app
                .daemon
                .get_query::<runnerd_core::api::JobPage>(
                    "/v1/jobs",
                    &runnerd_core::api::JobQuery {
                        limit: Some(8),
                        ..Default::default()
                    },
                )
                .await?
                .jobs;
        }
        if !open {
            return Ok(());
        }
    }
}

/// `GET /live/jobs/{id}`.
pub(crate) async fn job(
    State(app): State<App>,
    Path(id): Path<u64>,
    Query(signals): Query<Signals>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (patches, receiver) = mpsc::channel(64);
    tokio::spawn(async move {
        if let Err(error) = run_job(&app, &patches, id, signals.shown()).await {
            debug!("job stream ended: {error}");
            notice(&patches, &error).await;
        }
    });
    respond(receiver)
}

/// Appends log lines `first`, `first + 1`, … (counted from 0) to the `shown` lines of the page, and drops the
/// page's oldest beyond `LOG_TAIL`, so a page left open on a busy job keeps a bounded tail; the full log has the
/// rest.
async fn append_lines(patches: &Patches, first: u64, lines: &[String], shown: &mut u64) -> bool {
    if lines.is_empty() {
        return true;
    }
    let html: String = (first + 1..)
        .zip(lines)
        .map(|(number, line)| job::log_line(number, line).into_string())
        .collect();
    let append = PatchElements::new(html)
        .selector("#log-lines")
        .mode(ElementPatchMode::Append)
        .write_as_axum_sse_event();
    if patches.send(append).await.is_err() {
        return false;
    }
    *shown += lines.len() as u64;
    if *shown <= job::LOG_TAIL {
        return true;
    }
    *shown = job::LOG_TAIL;
    let trim = PatchElements::new("")
        .selector(format!(
            "#log-lines > li:nth-last-child(n+{})",
            job::LOG_TAIL + 1
        ))
        .mode(ElementPatchMode::Remove)
        .write_as_axum_sse_event();
    patches.send(trim).await.is_ok()
}

async fn run_job(app: &App, patches: &Patches, id: u64, shown: Option<Shown>) -> Result<(), Error> {
    let events = app
        .daemon
        .events(&EventQuery {
            job: Some(id),
            logs: true,
        })
        .await?;
    let mut events = Box::pin(events);
    let mut stopping = app.stopping.clone();
    let mut data = JobView::load(&app.daemon, id, crate::now()).await?;
    // The log's tail as it is now, in place of what the page has: whatever it got from the page's render or from
    // an earlier connection this one retries, each line shows once.
    let mut next_line = data.log_from + data.log.len() as u64;
    let mut shown_lines = data.log.len() as u64;
    let tail = PatchElements::new(job::log_lines(&data).into_string()).write_as_axum_sse_event();
    if patches.send(tail).await.is_err() {
        return Ok(());
    }
    let mut sent = Sent::default();
    if let Some(shown) = shown {
        data.now = shown.at;
        sent.seed(job::regions(&data), &shown);
    }
    loop {
        data.now = crate::now();
        if sent
            .update(patches, data.now, job::regions(&data))
            .await
            .is_err()
        {
            return Ok(());
        }
        if data.job.summary.state.is_final() {
            return Ok(());
        }
        let Some(first) = next_event(&mut events, patches, &mut stopping).await else {
            return Ok(());
        };
        let mut batch = vec![first];
        let open = settle(&mut events, &mut batch).await;
        let mut changed = false;
        let first_line = next_line;
        let mut lines = Vec::new();
        for event in batch {
            match event {
                Event::LogLine { line, text, .. } if line >= next_line => {
                    next_line = line + 1;
                    lines.push(text);
                }
                Event::JobSample { sample, .. } => data.add_sample(sample),
                Event::JobQueued { .. }
                | Event::JobStarted { .. }
                | Event::JobStep { .. }
                | Event::JobFinished { .. } => changed = true,
                _ => {}
            }
        }
        if !append_lines(patches, first_line, &lines, &mut shown_lines).await {
            return Ok(());
        }
        if changed {
            data.job = app.daemon.get::<Job>(&format!("/v1/jobs/{id}")).await?;
        }
        if !open {
            return Ok(());
        }
    }
}
