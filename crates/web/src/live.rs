//! Live pages: one SSE response per open page that sends Datastar element patches. Each subscribes to the daemon's
//! `/v1/events`, keeps the page's data current from the events (reading the daemon again where an event only says
//! that something changed), and re-renders the regions that changed, at most every `SETTLE`. Only what the page
//! lacks is sent: a region whose HTML differs, or when only some of its parts (`views::Parts`) do, those parts. On
//! connecting, the stream skips the regions the page already has, which the page names by digest in its `shown`
//! signal; after each update the stream sets that signal to what the page shows now, and the time, which live
//! charts follow. The stream ends when the page closes, its job finishes, the daemon answers with an error (shown as
//! a notice), or the dashboard stops. When the daemon's events end or have a gap, it fails instead, so the page
//! connects again and reads the daemon afresh.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use datastar::consts::Namespace;
use datastar::prelude::{ElementPatchMode, PatchElements, PatchSignals};
use futures_util::{Stream, StreamExt, stream};
use maud::Markup;
use runnerd_core::api::{Event, EventEnvelope, EventQuery, Job};
use runnerd_core::client::{ClientError, Events};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::App;
use crate::views::job::{self, JobView};
use crate::views::{self, Part, Parts, Tone, digest, overview};

/// How long to collect events before rendering: a burst (a slot and the budget change together) renders once.
const SETTLE: Duration = Duration::from_millis(250);

/// How old the overview's status may get while host samples arrive. Some of its changes have no event of their own
/// (`shutting_down`, a repository's queue listing failing or recovering), so it is read again this often.
const STATUS_AGE: Duration = Duration::from_secs(10);

type Patches = mpsc::Sender<SseEvent>;

/// How a page's stream ended.
enum End {
    /// As it should: the page closed, its job finished, or the dashboard is stopping.
    Done,
    /// The daemon's events stopped or have a gap: the page must connect again and read the daemon afresh.
    Lost,
}

/// The page's SSE response. Unless the task marked it done, the body ends in an error rather than cleanly: Datastar
/// retries a stream that fails, not one that finishes, and so the page connects again.
fn respond(
    receiver: mpsc::Receiver<SseEvent>,
    done: Arc<AtomicBool>,
) -> Sse<impl Stream<Item = Result<SseEvent, io::Error>>> {
    let events = stream::unfold(Some(receiver), move |receiver| {
        let done = done.clone();
        async move {
            let mut receiver = receiver?;
            match receiver.recv().await {
                Some(event) => Some((Ok(event), Some(receiver))),
                None if done.load(Ordering::Acquire) => None,
                None => Some((Err(io::Error::other("the daemon's events stopped")), None)),
            }
        }
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

/// Runs a page's stream and says how it ended, for `respond`.
async fn run_page(
    patches: Patches,
    done: Arc<AtomicBool>,
    what: &str,
    run: impl Future<Output = Result<End, ClientError>>,
) {
    match run.await {
        Ok(End::Done) => done.store(true, Ordering::Release),
        Ok(End::Lost) => debug!("{what} stream lost the daemon's events"),
        // A retry would most likely fail the same way and replace the notice, so the stream ends with it shown.
        Err(error) => {
            debug!("{what} stream ended: {error}");
            notice(&patches, &error).await;
            done.store(true, Ordering::Release);
        }
    }
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

/// The time a page was rendered at, and a digest of each region it had then, by id; on a job's page, also the
/// number of its last log line.
#[derive(Debug, Deserialize)]
struct Shown {
    at: u64,
    log: Option<u64>,
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
                        // One patch each: Datastar 1.0.4 drops every second element of an SVG patch.
                        (_, Some(_)) => events
                            .push(PatchElements::new(part.html.clone()).namespace(Namespace::Svg)),
                    }
                }
                if !html.is_empty() {
                    events.push(PatchElements::new(html));
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

async fn notice(patches: &Patches, error: &ClientError) {
    let event = PatchElements::new(views::notice(&error.to_string(), Tone::Failed).into_string())
        .write_as_axum_sse_event();
    let _ = patches.send(event).await;
}

/// The daemon's events as a stream, which ends when the reader fails or opens the stream again: live events (samples,
/// log lines) of the gap are lost even when it resumes, so the page connects again and reads the daemon afresh.
fn envelopes(events: Events) -> impl Stream<Item = EventEnvelope> {
    stream::unfold(events, |mut events| async move {
        let envelope = events.next().await.ok()?;
        (!events.reopened()).then_some((envelope, events))
    })
}

/// Waits `SETTLE` for more events, adding them to `batch`; `false` once the daemon's stream ended.
async fn settle(
    events: &mut (impl Stream<Item = EventEnvelope> + Unpin),
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

/// Waits for the next event; how the stream ends when the daemon's events, the page or the dashboard ended.
async fn next_event(
    events: &mut (impl Stream<Item = EventEnvelope> + Unpin),
    patches: &Patches,
    stopping: &mut watch::Receiver<bool>,
) -> Result<Event, End> {
    tokio::select! {
        event = events.next() => event.map(|envelope| envelope.event).ok_or(End::Lost),
        () = patches.closed() => Err(End::Done),
        _ = stopping.wait_for(|stopping| *stopping) => Err(End::Done),
    }
}

/// `GET /live/overview`.
pub(crate) async fn overview(
    State(app): State<App>,
    Query(signals): Query<Signals>,
) -> Sse<impl Stream<Item = Result<SseEvent, io::Error>>> {
    let (patches, receiver) = mpsc::channel(16);
    let done = Arc::new(AtomicBool::new(false));
    let ended = done.clone();
    tokio::spawn(async move {
        let run = run_overview(&app, &patches, signals.shown());
        run_page(patches.clone(), ended, "overview", run).await;
    });
    respond(receiver, done)
}

async fn run_overview(
    app: &App,
    patches: &Patches,
    shown: Option<Shown>,
) -> Result<End, ClientError> {
    // Subscribed before reading, so nothing that happens in between is missed.
    let events = app.daemon.events(EventQuery::default(), None).await?;
    let mut events = Box::pin(envelopes(events));
    let mut stopping = app.stopping.clone();
    let mut data = overview::Overview::load(&app.daemon, crate::now()).await?;
    let mut status_read = Instant::now();
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
            return Ok(End::Done);
        }
        let first = match next_event(&mut events, patches, &mut stopping).await {
            Ok(event) => event,
            Err(end) => return Ok(end),
        };
        let mut batch = vec![first];
        let open = settle(&mut events, &mut batch).await;
        let mut jobs_changed = false;
        let mut status_changed = false;
        for event in batch {
            match event {
                Event::JobSample { job, sample } => data.add_job_sample(job, sample),
                Event::HostSample { sample } => {
                    data.add_host_sample(sample);
                    status_changed |= status_read.elapsed() >= STATUS_AGE;
                }
                Event::LogLine { .. } | Event::CacheReseeded { .. } => {}
                Event::BudgetChanged { .. }
                | Event::ProfileChanged { .. }
                | Event::ProfileRemoved { .. }
                | Event::RepositoryChanged { .. }
                | Event::RepositoryRemoved { .. } => status_changed = true,
                Event::JobStep { .. } => jobs_changed = true,
                // The status's queue and slots follow jobs.
                Event::JobQueued { .. }
                | Event::JobStarted { .. }
                | Event::JobFinished { .. }
                | Event::SlotChanged { .. } => {
                    jobs_changed = true;
                    status_changed = true;
                }
            }
        }
        // Read again only when an event says it changed, or it is `STATUS_AGE` old: with a long queue the status is
        // large, and most batches are samples.
        if status_changed {
            data.status = app.daemon.get("/v1/status").await?;
            status_read = Instant::now();
        }
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
            return Ok(End::Lost);
        }
    }
}

/// `GET /live/jobs/{id}`.
pub(crate) async fn job(
    State(app): State<App>,
    Path(id): Path<u64>,
    Query(signals): Query<Signals>,
) -> Sse<impl Stream<Item = Result<SseEvent, io::Error>>> {
    let (patches, receiver) = mpsc::channel(64);
    let done = Arc::new(AtomicBool::new(false));
    let ended = done.clone();
    tokio::spawn(async move {
        let run = run_job(&app, &patches, id, signals.shown());
        run_page(patches.clone(), ended, "job", run).await;
    });
    respond(receiver, done)
}

/// Appends log lines `first`, `first + 1`, … (counted from 0) to the `shown` lines of the page, and drops the
/// page's oldest beyond `LOG_TAIL`, so a page left open on a busy job keeps a bounded tail; the full log has the
/// rest.
async fn append_lines(patches: &Patches, first: u64, lines: &[String], shown: &mut u64) -> bool {
    if lines.is_empty() {
        return true;
    }
    let last = first + lines.len() as u64;
    let html: String = (first + 1..)
        .zip(lines)
        .map(|(number, line)| {
            if number == last {
                job::last_log_line(number, line).into_string()
            } else {
                job::log_line(number, line).into_string()
            }
        })
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

async fn run_job(
    app: &App,
    patches: &Patches,
    id: u64,
    shown: Option<Shown>,
) -> Result<End, ClientError> {
    let events = app
        .daemon
        .events(
            EventQuery {
                job: Some(id),
                logs: true,
            },
            None,
        )
        .await?;
    let mut events = Box::pin(envelopes(events));
    let mut stopping = app.stopping.clone();
    let mut data = JobView::load(&app.daemon, id, crate::now()).await?;
    let mut next_line = data.log_from + data.log.len() as u64;
    // The lines after the page's last, which the page names, so each line shows once whether the page got it from
    // its render or from an earlier connection this one retries. A page that names none, or one too far behind,
    // gets the log's tail as it is now in place of its own.
    let page_end = shown
        .as_ref()
        .and_then(|shown| shown.log)
        .filter(|end| (data.log_from..=next_line).contains(end));
    let mut shown_lines = data.log.len() as u64;
    let sent_log = if let Some(end) = page_end {
        shown_lines = end.min(job::LOG_TAIL);
        let from = usize::try_from(end - data.log_from).unwrap_or(usize::MAX);
        let lines = data.log.get(from..).unwrap_or_default();
        append_lines(patches, end, lines, &mut shown_lines).await
    } else {
        let tail = PatchElements::new(job::log_lines(&data).into_string());
        patches.send(tail.write_as_axum_sse_event()).await.is_ok()
    };
    if !sent_log {
        return Ok(End::Done);
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
            return Ok(End::Done);
        }
        if data.job.summary.state.is_final() {
            return Ok(End::Done);
        }
        let first = match next_event(&mut events, patches, &mut stopping).await {
            Ok(event) => event,
            Err(end) => return Ok(end),
        };
        let mut batch = vec![first];
        let open = settle(&mut events, &mut batch).await;
        let mut changed = false;
        let first_line = next_line;
        let mut lines = Vec::new();
        for event in batch {
            match event {
                // A line skipped means the daemon dropped some for this stream: the page connects again and gets
                // them from the stored log.
                Event::LogLine { line, .. } if line > next_line => return Ok(End::Lost),
                Event::LogLine { line, text, .. } if line == next_line => {
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
            return Ok(End::Done);
        }
        if changed {
            data.job = app.daemon.get::<Job>(&format!("/v1/jobs/{id}")).await?;
        }
        if !open {
            return Ok(End::Lost);
        }
    }
}
