//! Live pages: one SSE response per open page that sends Datastar element patches. Each subscribes to the daemon's
//! `/v1/events`, keeps the page's data current from the events (reading the daemon again where an event only says
//! that something changed), and re-renders the regions that changed, at most every `SETTLE`. Only what the page
//! lacks is sent: a region whose HTML differs, or when only some of its parts (`views::Parts`) do, those parts. On
//! connecting, the stream skips the regions the page already has, which the page names by digest in its `shown`
//! signal; after each update the stream sets that signal to what the page shows now, and the time, which live
//! charts follow. A read that fails as the daemon is away is tried again with a growing wait, the page keeping what
//! it shows, and the page's feed (`feed`, ADR 0177) says so; each delivery of the daemon's data, never a keep-alive,
//! sets when the page's data is from. The stream ends when the page closes, its job finishes, the daemon answers with
//! an error (shown as a notice), every device is signed out (also a notice), or the dashboard stops; the feed says
//! which, so the page never claims to retry a stream that ended. When the daemon's events end or have a gap, it
//! fails instead, so the page connects again and reads the daemon afresh.

pub(crate) mod feed;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use datastar::consts::Namespace;
use datastar::prelude::{ElementPatchMode, PatchElements, PatchSignals};
use futures_util::{Stream, StreamExt, stream};
use maud::Markup;
use runnerd_core::api::{Event, EventEnvelope, EventQuery, Job};
use runnerd_core::client::{ClientError, Events};

use self::feed::{Ending, Feed};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::App;
use crate::auth::SignedIn;
use crate::views::job::{self, JobView};
use crate::views::local_pages::LocalPages;
use crate::views::queue::{self, QueueView};
use crate::views::{self, Part, Parts, Tone, digest, overview};

/// How long to collect events before rendering: a burst (a slot and the budget change together) renders once.
const SETTLE: Duration = Duration::from_millis(250);

/// How old the overview's status may get while host samples arrive, and the queue page's at most. Some of its
/// changes have no event of their own (`shutting_down`, a repository's queue listing failing or recovering), so it is
/// read again this often. A second in tests, which wait for that.
const STATUS_AGE: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });

/// How old the overview's health figures may get while other events arrive: their 24 hours move on without a job
/// event, and jobs age out of them. A second in tests, which wait for that.
const DAY_AGE: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 60 });

/// How often a page's stream sends a keep-alive comment, which keeps the connection open and delivers nothing. A
/// second in tests, which wait for one.
const KEEP_ALIVE: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 15 });

type Patches = mpsc::Sender<SseEvent>;

/// How a page's stream ended. A page's run returns only these, as its `Err`.
enum End {
    /// The page closed.
    Closed,
    /// For good, as the feed says: the job finished or the dashboard is stopping.
    Ended(Ending),
    /// The daemon answered a read with an error, which the page shows; a retry would most likely fail the same way.
    Refused(ClientError),
    /// The daemon's events stopped or have a gap: the page must connect again and read the daemon afresh.
    Lost,
}

/// A page's run: it ends only by an `End`.
type Run = Result<std::convert::Infallible, End>;

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
    Sse::new(events).keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
}

/// A live page.
#[derive(Debug, Clone, Copy)]
enum LivePage {
    Overview,
    Queue,
    Job(u64),
}

impl LivePage {
    fn name(self) -> &'static str {
        match self {
            LivePage::Overview => "overview",
            LivePage::Queue => "queue",
            LivePage::Job(_) => "job",
        }
    }
}

/// Runs `page`'s stream for `session`, with the feed of a page whose data is from `shown`, and says how it ended, for
/// `respond`. Signing every device out ends it with a notice, as a retry would be refused.
async fn run_page(
    app: App,
    patches: Patches,
    done: Arc<AtomicBool>,
    session: SignedIn,
    page: LivePage,
    shown: Option<Shown>,
) {
    let what = page.name();
    let mut feed = Feed::connecting(shown.as_ref().map(|shown| shown.at));
    if send_feed(&patches, &feed).await.is_err() {
        done.store(true, Ordering::Release);
        return;
    }
    let ended = {
        let feed = &mut feed;
        let run = async {
            let Err(end) = match page {
                LivePage::Overview => run_overview(&app, &patches, feed, shown).await,
                LivePage::Queue => run_queue(&app, &patches, feed, shown).await,
                LivePage::Job(id) => run_job(&app, &patches, feed, id, shown).await,
            };
            end
        };
        tokio::select! {
            end = run => Some(end),
            () = session.signed_out() => None,
        }
    };
    let ending = match ended {
        None => {
            debug!("{what} stream ended: every device was signed out");
            let notice = views::notice("This browser was signed out", Tone::Failed).into_string();
            let _ = patches
                .send(PatchElements::new(notice).write_as_axum_sse_event())
                .await;
            Ending::SignedOut
        }
        Some(End::Closed) => {
            done.store(true, Ordering::Release);
            return;
        }
        Some(End::Ended(why)) => why,
        Some(End::Lost) => {
            debug!("{what} stream lost the daemon's events");
            return;
        }
        // A retry would most likely fail the same way and replace the notice, so the stream ends with it shown.
        Some(End::Refused(error)) => {
            debug!("{what} stream ended: {error}");
            notice(&patches, &error).await;
            Ending::Refused
        }
    };
    feed.end(ending);
    let _ = send_feed(&patches, &feed).await;
    done.store(true, Ordering::Release);
}

/// Tells the page what its feed is now.
async fn send_feed(patches: &Patches, feed: &Feed) -> Result<(), End> {
    patches
        .send(PatchSignals::new(feed.patch()).write_as_axum_sse_event())
        .await
        .map_err(|_| End::Closed)
}

/// Reads the daemon with the future `$read` makes, made again for each try, until it answers. A read that fails as
/// the daemon is away is tried again after the feed's wait (`retry`); an error the daemon answers ends the stream with
/// it. A macro, not a function taking a closure: each read borrows the page's data, which a closure's future cannot
/// hold across tries.
macro_rules! read {
    ($feed:expr, $patches:expr, $stopping:expr, $read:expr) => {
        loop {
            match $read.await {
                Ok(value) => break Ok(value),
                Err(error) => {
                    if let Err(end) = retry($feed, $patches, $stopping, error).await {
                        break Err(end);
                    }
                }
            }
        }
    };
}

/// After a read failed with `error`: when the daemon is away, the feed says the page retries, keeping what it shows,
/// and this waits the feed's wait; else the stream ends with the error. The page closing or the dashboard stopping
/// ends the wait.
async fn retry(
    feed: &mut Feed,
    patches: &Patches,
    stopping: &mut watch::Receiver<bool>,
    error: ClientError,
) -> Result<(), End> {
    if !error.is_outage() {
        return Err(End::Refused(error));
    }
    debug!("a live read failed, trying again: {error}");
    let wait = feed.failed(&error);
    send_feed(patches, feed).await?;
    tokio::select! {
        () = tokio::time::sleep(wait) => Ok(()),
        () = patches.closed() => Err(End::Closed),
        _ = stopping.wait_for(|stopping| *stopping) => Err(End::Ended(Ending::Stopped)),
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
    /// that changed. The feed takes it as a delivery of the daemon's data at `at`, and the signal says so too.
    async fn update(
        &mut self,
        patches: &Patches,
        feed: &mut Feed,
        at: u64,
        regions: impl IntoIterator<Item = (&'static str, Markup, Parts)>,
    ) -> Result<(), End> {
        let mut shown = serde_json::Map::new();
        shown.insert("at".to_owned(), at.into());
        for (id, markup, parts) in regions {
            if self
                .region(patches, id, Rendered::new(markup, parts))
                .await
                .map_err(|Closed| End::Closed)?
            {
                let digest = self.0.get(id).map(|sent| digest(&sent.html));
                shown.insert(id.to_owned(), digest.unwrap_or_default().into());
            }
        }
        feed.delivered(at);
        let signals = serde_json::json!({ "shown": shown, "_feed": feed.json() }).to_string();
        patches
            .send(PatchSignals::new(signals).write_as_axum_sse_event())
            .await
            .map_err(|_| End::Closed)
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
        () = patches.closed() => Err(End::Closed),
        _ = stopping.wait_for(|stopping| *stopping) => Err(End::Ended(Ending::Stopped)),
    }
}

/// A read of the overview's that a batch of events may call for, each on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Again {
    Status,
    Jobs,
    Queue,
    Day,
    Attention,
}

/// What the overview reads again after a batch of events.
#[derive(Debug, Default)]
struct Stale(std::collections::BTreeSet<Again>);

impl Stale {
    /// Adds the batch's samples to `data`, and says what to read again: what an event says changed, the status when a
    /// host sample comes and it is `status_old`, and the day's figures when they are `day_old`.
    fn of(
        batch: Vec<Event>,
        data: &mut overview::Overview,
        status_old: bool,
        day_old: bool,
    ) -> Stale {
        let mut stale = Stale::default();
        if day_old {
            stale.0.insert(Again::Day);
        }
        for event in batch {
            match event {
                Event::JobSample { job, sample } => data.add_job_sample(job, sample),
                Event::HostSample { sample } => {
                    data.add_host_sample(sample);
                    if status_old {
                        stale.0.insert(Again::Status);
                    }
                }
                Event::AttentionChanged { .. } => {
                    stale.0.insert(Again::Attention);
                }
                Event::LogLine { .. }
                | Event::CacheReseeded { .. }
                | Event::RecordCreated { .. }
                | Event::RecordDeleted { .. }
                | Event::RecordGone { .. }
                | Event::RecordSkipped { .. }
                | Event::SizingChanged { .. }
                | Event::RunnerFailed { .. } => {}
                Event::BudgetChanged { .. }
                | Event::ProfileChanged { .. }
                | Event::ProfileRemoved { .. }
                | Event::RepositoryChanged { .. }
                | Event::RepositoryRemoved { .. } => {
                    stale.0.insert(Again::Status);
                }
                Event::JobStep { .. } => {
                    stale.0.insert(Again::Jobs);
                }
                // The status's queue and slots follow jobs.
                Event::SlotChanged { .. } => {
                    stale.0.insert(Again::Jobs);
                    stale.0.insert(Again::Queue);
                    stale.0.insert(Again::Status);
                }
                // The health figures count the day's jobs too.
                Event::JobQueued { .. } | Event::JobStarted { .. } | Event::JobFinished { .. } => {
                    stale.0.insert(Again::Jobs);
                    stale.0.insert(Again::Queue);
                    stale.0.insert(Again::Status);
                    stale.0.insert(Again::Day);
                }
            }
        }
        stale
    }

    fn has(&self, read: Again) -> bool {
        self.0.contains(&read)
    }
}

/// `GET /live/overview`.
pub(crate) async fn overview(
    State(app): State<App>,
    Extension(session): Extension<SignedIn>,
    Query(signals): Query<Signals>,
) -> Sse<impl Stream<Item = Result<SseEvent, io::Error>>> {
    let (patches, receiver) = mpsc::channel(16);
    let done = Arc::new(AtomicBool::new(false));
    let ended = done.clone();
    tokio::spawn(async move {
        run_page(
            app,
            patches,
            ended,
            session,
            LivePage::Overview,
            signals.shown(),
        )
        .await;
    });
    respond(receiver, done)
}

async fn run_overview(app: &App, patches: &Patches, feed: &mut Feed, shown: Option<Shown>) -> Run {
    let mut stopping = app.stopping.clone();
    // Subscribed before reading, so nothing that happens in between is missed.
    let events = read!(
        feed,
        patches,
        &mut stopping,
        app.daemon.events(EventQuery::default(), None)
    )?;
    let mut events = Box::pin(envelopes(events));
    let mut data = read!(
        feed,
        patches,
        &mut stopping,
        overview::Overview::load(&app.daemon, crate::now())
    )?;
    let mut status_read = Instant::now();
    let mut day_read = Instant::now();
    let mut sent = Sent::default();
    if let Some(shown) = shown {
        data.now = shown.at;
        sent.seed(overview::regions(&data), &shown);
    }
    loop {
        data.now = crate::now();
        sent.update(patches, feed, data.now, overview::regions(&data))
            .await?;
        let first = next_event(&mut events, patches, &mut stopping).await?;
        let mut batch = vec![first];
        let open = settle(&mut events, &mut batch).await;
        let stale = Stale::of(
            batch,
            &mut data,
            status_read.elapsed() >= STATUS_AGE,
            day_read.elapsed() >= DAY_AGE,
        );
        // Read again only when an event says it changed, or it is `STATUS_AGE` old: with a long queue the status is
        // large, and most batches are samples.
        if stale.has(Again::Status) {
            data.status = read!(feed, patches, &mut stopping, app.daemon.get("/v1/status"))?;
            status_read = Instant::now();
        }
        if stale.has(Again::Jobs) {
            read!(
                feed,
                patches,
                &mut stopping,
                data.reload_running(&app.daemon)
            )?;
            data.recent = read!(
                feed,
                patches,
                &mut stopping,
                overview::read_recent(&app.daemon, &data.status)
            )?;
        }
        // Which queued jobs have a page: again when the queue may have changed, never for a step alone, and when the
        // status shows a job the last read did not ask about or that read failed.
        if stale.has(Again::Queue) || data.local.stale(&data.status) {
            data.local = LocalPages::read(&app.daemon, &data.status).await;
        }
        if stale.has(Again::Attention) {
            data.attention = read!(feed, patches, &mut stopping, app.daemon.attention())?;
        }
        if stale.has(Again::Day) {
            read!(
                feed,
                patches,
                &mut stopping,
                data.reload_day(&app.daemon, crate::now())
            )?;
            day_read = Instant::now();
        }
        if !open {
            return Err(End::Lost);
        }
    }
}

/// `GET /live/queue`.
pub(crate) async fn queue(
    State(app): State<App>,
    Extension(session): Extension<SignedIn>,
    Query(signals): Query<Signals>,
) -> Sse<impl Stream<Item = Result<SseEvent, io::Error>>> {
    let (patches, receiver) = mpsc::channel(16);
    let done = Arc::new(AtomicBool::new(false));
    let ended = done.clone();
    tokio::spawn(async move {
        run_page(
            app,
            patches,
            ended,
            session,
            LivePage::Queue,
            signals.shown(),
        )
        .await;
    });
    respond(receiver, done)
}

/// The queue's page: the status is read again when an event says the queue or what it waits on changed, and at the
/// latest once it is `STATUS_AGE` old, event or not; every read renders the waits as they have grown. Which jobs have
/// a page is read again with a job event, and when the status shows a job the last read did not ask about or that
/// read failed.
async fn run_queue(app: &App, patches: &Patches, feed: &mut Feed, shown: Option<Shown>) -> Run {
    let mut stopping = app.stopping.clone();
    // Subscribed before reading, so nothing that happens in between is missed.
    let events = read!(
        feed,
        patches,
        &mut stopping,
        app.daemon.events(EventQuery::default(), None)
    )?;
    let mut events = Box::pin(envelopes(events));
    let mut data = read!(
        feed,
        patches,
        &mut stopping,
        QueueView::load(&app.daemon, crate::now())
    )?;
    let mut status_read = Instant::now();
    let mut sent = Sent::default();
    if let Some(shown) = shown {
        data.now = shown.at;
        sent.seed(queue::regions(&data), &shown);
    }
    loop {
        data.now = crate::now();
        sent.update(patches, feed, data.now, queue::regions(&data))
            .await?;
        // The status is read again by `STATUS_AGE` whether or not an event comes: a host whose slice cannot be read
        // sends no samples.
        let deadline = tokio::time::sleep_until((status_read + STATUS_AGE).into());
        let first = tokio::select! {
            event = next_event(&mut events, patches, &mut stopping) => Some(event?),
            () = deadline => None,
        };
        let mut batch: Vec<Event> = first.into_iter().collect();
        let open = batch.is_empty() || settle(&mut events, &mut batch).await;
        let mut status_changed = status_read.elapsed() >= STATUS_AGE;
        let mut jobs_changed = false;
        for event in batch {
            match event {
                Event::JobQueued { .. } | Event::JobStarted { .. } | Event::JobFinished { .. } => {
                    status_changed = true;
                    jobs_changed = true;
                }
                Event::SlotChanged { .. }
                | Event::BudgetChanged { .. }
                | Event::ProfileChanged { .. }
                | Event::ProfileRemoved { .. }
                | Event::RepositoryChanged { .. }
                | Event::RepositoryRemoved { .. }
                | Event::CacheReseeded { .. }
                | Event::RunnerFailed { .. }
                | Event::AttentionChanged { .. } => status_changed = true,
                Event::JobSample { .. }
                | Event::HostSample { .. }
                | Event::LogLine { .. }
                | Event::JobStep { .. }
                | Event::RecordCreated { .. }
                | Event::RecordDeleted { .. }
                | Event::RecordGone { .. }
                | Event::RecordSkipped { .. }
                | Event::SizingChanged { .. } => {}
            }
        }
        if status_changed {
            data.status = read!(feed, patches, &mut stopping, app.daemon.get("/v1/status"))?;
            status_read = Instant::now();
        }
        if jobs_changed || data.local.stale(&data.status) {
            data.local = LocalPages::read(&app.daemon, &data.status).await;
        }
        if !open {
            return Err(End::Lost);
        }
    }
}

/// `GET /live/jobs/{id}`.
pub(crate) async fn job(
    State(app): State<App>,
    Extension(session): Extension<SignedIn>,
    Path(id): Path<u64>,
    Query(signals): Query<Signals>,
) -> Sse<impl Stream<Item = Result<SseEvent, io::Error>>> {
    let (patches, receiver) = mpsc::channel(64);
    let done = Arc::new(AtomicBool::new(false));
    let ended = done.clone();
    tokio::spawn(async move {
        run_page(
            app,
            patches,
            ended,
            session,
            LivePage::Job(id),
            signals.shown(),
        )
        .await;
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
    feed: &mut Feed,
    id: u64,
    shown: Option<Shown>,
) -> Run {
    let mut stopping = app.stopping.clone();
    let query = EventQuery {
        job: Some(id),
        logs: true,
    };
    let events = read!(
        feed,
        patches,
        &mut stopping,
        app.daemon.events(query.clone(), None)
    )?;
    let mut events = Box::pin(envelopes(events));
    let mut data = read!(
        feed,
        patches,
        &mut stopping,
        JobView::load(&app.daemon, id, crate::now(), &job::JobPageQuery::default())
    )?;
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
        return Err(End::Closed);
    }
    let mut sent = Sent::default();
    if let Some(shown) = shown {
        data.now = shown.at;
        sent.seed(job::regions(&data), &shown);
    }
    loop {
        data.now = crate::now();
        sent.update(patches, feed, data.now, job::regions(&data))
            .await?;
        if data.job.summary.state.is_final() {
            // The parts a running job's page lacks: its failure, its outlined log, the comparison and the record.
            let full = read!(
                feed,
                patches,
                &mut stopping,
                JobView::load(&app.daemon, id, crate::now(), &job::JobPageQuery::default())
            )?;
            let finished = PatchElements::new(job::finished(&full).into_string());
            let _ = patches.send(finished.write_as_axum_sse_event()).await;
            return Err(End::Ended(Ending::Finished));
        }
        let first = next_event(&mut events, patches, &mut stopping).await?;
        let mut batch = vec![first];
        let open = settle(&mut events, &mut batch).await;
        let mut changed = false;
        let first_line = next_line;
        let mut lines = Vec::new();
        for event in batch {
            match event {
                // A line skipped means the daemon dropped some for this stream: the page connects again and gets
                // them from the stored log.
                Event::LogLine { line, .. } if line > next_line => return Err(End::Lost),
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
            return Err(End::Closed);
        }
        if changed {
            let job = read!(
                feed,
                patches,
                &mut stopping,
                app.daemon.get::<Job>(&format!("/v1/jobs/{id}"))
            )?;
            data.job.reread(job);
        }
        if !open {
            return Err(End::Lost);
        }
    }
}
