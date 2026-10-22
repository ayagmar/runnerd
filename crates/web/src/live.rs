//! Live pages: one SSE response per open page that sends Datastar element patches. Each subscribes to the daemon's
//! `/v1/events`, keeps the page's data current from the events (reading the daemon again where an event only says
//! that something changed), and re-renders the regions that changed, at most every `SETTLE`. A region is sent only
//! when its HTML differs from what the page has. The stream ends when the page closes, the daemon's stream ends,
//! or the dashboard stops.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use datastar::prelude::{ElementPatchMode, PatchElements};
use futures_util::{Stream, StreamExt, stream};
use maud::Markup;
use runnerd_core::api::{Event, EventQuery, Job};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::App;
use crate::daemon::Error;
use crate::views::job::{self, JobView};
use crate::views::{self, overview};

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

/// The regions a page has, by id, so unchanged ones are not sent again.
#[derive(Default)]
struct Sent(HashMap<&'static str, String>);

impl Sent {
    /// Sends the region unless the page has it already; `false` once the page is gone.
    async fn region(&mut self, patches: &Patches, id: &'static str, markup: Markup) -> bool {
        let html = markup.into_string();
        if self.0.get(id) == Some(&html) {
            return true;
        }
        let event = PatchElements::new(html.clone()).write_as_axum_sse_event();
        self.0.insert(id, html);
        patches.send(event).await.is_ok()
    }
}

async fn notice(patches: &Patches, error: &Error) {
    let event = PatchElements::new(views::notice(&error.to_string(), true).into_string())
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
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (patches, receiver) = mpsc::channel(16);
    tokio::spawn(async move {
        if let Err(error) = run_overview(&app, &patches).await {
            debug!("overview stream ended: {error}");
            notice(&patches, &error).await;
        }
    });
    respond(receiver)
}

async fn run_overview(app: &App, patches: &Patches) -> Result<(), Error> {
    // Subscribed before reading, so nothing that happens in between is missed.
    let events = app.daemon.events(&EventQuery::default()).await?;
    let mut events = Box::pin(events);
    let mut stopping = app.stopping.clone();
    let mut data = overview::Overview::load(&app.daemon, crate::now()).await?;
    let mut sent = Sent::default();
    loop {
        data.now = crate::now();
        let regions = [
            ("budget", overview::budget(&data)),
            ("slots", overview::slots(&data)),
            ("host", overview::host(&data)),
            ("repositories", overview::repositories(&data.status)),
            ("recent", overview::recent(&data)),
        ];
        for (id, markup) in regions {
            if !sent.region(patches, id, markup).await {
                return Ok(());
            }
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
                | Event::BudgetChanged { .. } => {}
                Event::JobQueued { .. }
                | Event::JobStarted { .. }
                | Event::JobStep { .. }
                | Event::JobFinished { .. }
                | Event::SlotChanged { .. } => jobs_changed = true,
            }
        }
        // Queue lengths and pauses change without an event; the status is cheap to read on every update.
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

#[derive(Debug, Deserialize)]
pub(crate) struct JobLive {
    /// The number of log lines the page shows already.
    #[serde(default)]
    from: u64,
}

/// `GET /live/jobs/{id}?from=…`.
pub(crate) async fn job(
    State(app): State<App>,
    Path(id): Path<u64>,
    Query(live): Query<JobLive>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (patches, receiver) = mpsc::channel(64);
    tokio::spawn(async move {
        if let Err(error) = run_job(&app, &patches, id, live.from).await {
            debug!("job stream ended: {error}");
            notice(&patches, &error).await;
        }
    });
    respond(receiver)
}

async fn append_lines(patches: &Patches, lines: &[String]) -> bool {
    if lines.is_empty() {
        return true;
    }
    let html: String = lines
        .iter()
        .map(|line| job::log_line(line).into_string())
        .collect();
    let event = PatchElements::new(html)
        .selector("#log-lines")
        .mode(ElementPatchMode::Append)
        .write_as_axum_sse_event();
    patches.send(event).await.is_ok()
}

async fn run_job(app: &App, patches: &Patches, id: u64, from: u64) -> Result<(), Error> {
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
    // Lines stored between the page's render and this stream's start.
    let missed = app
        .daemon
        .text(&format!("/v1/jobs/{id}/logs?from={from}"))
        .await?;
    let missed: Vec<String> = missed.lines().map(str::to_owned).collect();
    let mut next_line = from + missed.len() as u64;
    if !append_lines(patches, &missed).await {
        return Ok(());
    }
    let mut sent = Sent::default();
    loop {
        data.now = crate::now();
        let regions = [
            ("job-head", job::head(&data)),
            ("timeline", job::timeline(&data)),
            ("steps", job::steps(&data)),
            ("metrics", job::metrics(&data)),
        ];
        for (id, markup) in regions {
            if !sent.region(patches, id, markup).await {
                return Ok(());
            }
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
        if !append_lines(patches, &lines).await {
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
