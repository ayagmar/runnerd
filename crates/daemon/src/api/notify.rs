//! Notifications (ADR 0051, 0064): under `[notify]`, sends failed and lost jobs, runners that cannot start, new
//! warning insights and the daily digest to each target through its kind's `send::Sender`. A reader of the store's durable events beside auto
//! sizing, off the engine's loop: the engine never waits for it. Each target's cursor, the insights it was told and
//! its last digest are stored once a delivery succeeds, so a restart drops no message, and repeats one only if it stops
//! between sending and storing: at least once. It folds a burst into one message, keeping the first `KEEP` of each
//! kind and counting the rest, retries with backoff, and never logs or returns a target's URL or header values.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use runnerd_core::api::{
    Delivery, Digest, Event, EventEnvelope, Insight, JobState, JobSummary, Notification,
    NotifyEvent, NotifyTest, NotifyTestQuery, Priority, RunnerFailure, Severity, job_title,
};
use runnerd_core::config::{Config, DayTime, Notify, NotifyTarget};
use runnerd_core::insights;
use runnerd_core::store::Store;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, broadcast, watch};
use tokio::time::Instant;
use tracing::{info, warn};

use self::send::{client, deliver};
use super::history::{insight_runs_of, now, savings_of};
use super::{AppState, Authenticated, ConfigText, error};

mod discord;
mod ntfy;
mod send;
mod webhook;

/// How the notifier paces itself: `TIMING` in the daemon, shorter in tests.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How long after the first event of a message it waits for more to fold in.
    pub gather: Duration,
    /// The least time between two messages to one target.
    pub gap: Duration,
    /// The wait before the first retry; each next one doubles, up to `retry_max`.
    pub retry: Duration,
    /// The longest wait between retries.
    pub retry_max: Duration,
    /// How often insights and the digest's time are looked at.
    pub check: Duration,
    /// How long a request may take.
    pub timeout: Duration,
}

/// The daemon's pace.
pub(crate) const TIMING: Timing = Timing {
    gather: Duration::from_secs(10),
    gap: Duration::from_mins(1),
    retry: Duration::from_secs(5),
    retry_max: Duration::from_mins(10),
    check: Duration::from_mins(5),
    timeout: Duration::from_secs(10),
};

/// Reads the host's offset from UTC, in seconds: `local_offset`, or a test's.
pub(crate) type LocalOffset = fn() -> Option<i64>;

/// The host's offset from UTC now, in seconds, as `date +%z` prints it, so it follows the zone and daylight saving;
/// `None` when it cannot be read, and the digest waits for a look that can.
pub(crate) fn local_offset() -> Option<i64> {
    let output = std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()?;
    parse_offset(String::from_utf8_lossy(&output.stdout).trim())
}

/// `+0200` or `-0530` in seconds.
fn parse_offset(text: &str) -> Option<i64> {
    let (sign, digits) = match text.split_at_checked(1)? {
        ("+", digits) => (1, digits),
        ("-", digits) => (-1, digits),
        _ => return None,
    };
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Whether the digest due at `at` local time is due at `now` (seconds) in a zone `offset` seconds from UTC, when the
/// last went at `last`: once a local day, at the first look after its time. One that went earlier today counts, so
/// moving `at` later does not send a second.
fn digest_due(now: u64, offset: i64, at: DayTime, last: u64) -> bool {
    const DAY: i64 = 24 * 3600;
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let midnight = (now + offset).div_euclid(DAY) * DAY - offset;
    let due = midnight + i64::from(at.minutes) * 60;
    now >= due && i64::try_from(last).unwrap_or(i64::MAX) < midnight
}

/// What the notifier needs: `TIMING` and `local_offset` in the daemon.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Setup {
    /// Its pace.
    pub timing: Timing,
    /// Reads the host's offset from UTC.
    pub offset: LocalOffset,
}

/// The daemon's setup.
pub(crate) const SETUP: Setup = Setup {
    timing: TIMING,
    offset: local_offset,
};

const DAY_MS: u64 = 24 * 3600 * 1000;

/// How many jobs or runners of each kind a message names and carries; the rest are counted. Bounds what a target
/// keeps while its endpoint refuses, and the body it is sent.
const KEEP: usize = 20;

/// Things of one kind waiting to be sent: the first `KEEP`, and how many came.
struct Kept<T> {
    first: Vec<T>,
    count: usize,
}

impl<T> Default for Kept<T> {
    fn default() -> Self {
        Kept {
            first: Vec::new(),
            count: 0,
        }
    }
}

impl<T> Kept<T> {
    fn push(&mut self, item: T) {
        if self.first.len() < KEEP {
            self.first.push(item);
        }
        self.count += 1;
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// One target's state: what it was sent, what waits to be, and when it may be sent.
struct Target {
    config: NotifyTarget,
    /// The last event read; delivered, or not wanted.
    cursor: u64,
    /// The cursor as stored.
    stored: u64,
    failed: Kept<JobSummary>,
    lost: Kept<JobSummary>,
    runners: Kept<RunnerFailure>,
    /// New warning insights to tell, and when the warning insights held at the last look (ms).
    insights: Vec<Insight>,
    present: Option<(u64, BTreeSet<String>)>,
    /// When each insight told of was last seen to hold (ms), by key.
    told: BTreeMap<String, u64>,
    /// Whether it has looked since the daemon started: until then, `told` is as an earlier run last saw it.
    looked: bool,
    digest: Option<Digest>,
    /// When the last digest went (ms).
    last_digest: u64,
    /// When the oldest thing waiting to be sent came.
    since: Option<Instant>,
    sent: Option<Instant>,
    retry_at: Option<Instant>,
    failures: u32,
}

impl Target {
    fn wants(&self, event: NotifyEvent) -> bool {
        self.config.events.contains(&event)
    }

    fn waiting(&self) -> bool {
        !(self.failed.is_empty()
            && self.lost.is_empty()
            && self.runners.is_empty()
            && self.insights.is_empty())
    }

    /// When it may send next, if it has something to.
    fn due(&self, timing: &Timing) -> Option<Instant> {
        let earliest = [
            self.since.map(|since| since + timing.gather),
            self.sent.map(|sent| sent + timing.gap),
            self.retry_at,
        ]
        .into_iter()
        .flatten()
        .max();
        (self.waiting() || self.digest.is_some()).then(|| earliest.unwrap_or_else(Instant::now))
    }

    /// Takes in a stored event the target wants.
    fn take(&mut self, event: Event) {
        let before = self.waiting();
        match event {
            Event::JobFinished { job }
                if job.state == JobState::Lost && self.wants(NotifyEvent::Lost) =>
            {
                self.lost.push(job);
            }
            Event::JobFinished { job }
                if job.state == JobState::Completed
                    && job.is_failure()
                    && self.wants(NotifyEvent::Failed) =>
            {
                self.failed.push(job);
            }
            Event::RunnerFailed { failure } if self.wants(NotifyEvent::Runner) => {
                self.runners.push(failure);
            }
            _ => {}
        }
        if !before && self.waiting() {
            self.since = Some(Instant::now());
        }
    }

    /// Takes in the warning insights that hold at `now` (ms): those not told, or gone for a day since, wait to be.
    fn look(&mut self, warnings: &[Insight], now: u64) {
        let before = self.waiting();
        // A daemon that was off did not see any go away: what was told before counts as seen at the first look.
        if !self.looked {
            self.looked = true;
            for seen in self.told.values_mut() {
                *seen = (*seen).max(now);
            }
        }
        self.insights = warnings
            .iter()
            .filter(|insight| {
                self.told
                    .get(&insight.key)
                    .is_none_or(|seen| *seen + DAY_MS < now)
            })
            .cloned()
            .collect();
        self.present = Some((
            now,
            warnings.iter().map(|insight| insight.key.clone()).collect(),
        ));
        if !before && self.waiting() {
            self.since = Some(Instant::now());
        }
    }

    /// What to store once the insights looked at are told: each held one seen now, and those gone for a day
    /// forgotten.
    fn told_now(&mut self) -> (Vec<(String, u64)>, Vec<String>) {
        let Some((at, present)) = self.present.take() else {
            return (Vec::new(), Vec::new());
        };
        for key in &present {
            self.told.insert(key.clone(), at);
        }
        let gone: Vec<String> = self
            .told
            .iter()
            .filter(|(key, seen)| !present.contains(*key) && **seen + DAY_MS < at)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &gone {
            self.told.remove(key);
        }
        let set = present
            .into_iter()
            .map(|key| (format!("insight:{key}"), at))
            .collect();
        (
            set,
            gone.into_iter()
                .map(|key| format!("insight:{key}"))
                .collect(),
        )
    }

    fn failed_to_send(&mut self, timing: &Timing, why: &str) {
        let wait = self.back_off(timing);
        warn!(
            "notify {}: not delivered: {why}; retrying in {}s",
            self.config.name,
            wait.as_secs()
        );
    }

    /// Waits longer after each failure in a row, to send or to read the history.
    fn back_off(&mut self, timing: &Timing) -> Duration {
        self.failures += 1;
        let wait = timing
            .retry
            .saturating_mul(1 << self.failures.saturating_sub(1).min(16))
            .min(timing.retry_max);
        self.retry_at = Some(Instant::now() + wait);
        wait
    }

    fn delivered(&mut self) {
        self.sent = Some(Instant::now());
        self.retry_at = None;
        self.failures = 0;
    }
}

/// The notifier's view of the daemon.
pub(super) struct Notifier {
    pub store: Store,
    pub config: Arc<Mutex<ConfigText>>,
    /// Signalled each time `config` is replaced, so a target added starts from then, not from the next event.
    pub replaced: Arc<tokio::sync::Notify>,
    pub stopping: watch::Receiver<bool>,
    pub setup: Setup,
}

/// Sends what `[notify]` asks for until the server stops; reads `[notify]` again at every wake and as soon as the
/// config is replaced, so a reload takes effect without a restart.
pub(super) async fn run(notifier: Notifier) {
    let Notifier {
        store,
        config,
        replaced,
        mut stopping,
        setup,
    } = notifier;
    let http = match client(setup.timing.timeout) {
        Ok(http) => http,
        Err(why) => {
            warn!("notify: cannot make an HTTP client, so nothing is sent: {why}");
            return;
        }
    };
    let mut live = store.subscribe();
    let mut stored = match store.notify_state().await {
        Ok(stored) => stored,
        Err(store_error) => {
            warn!("notify: cannot read what was sent, so nothing is: {store_error}");
            return;
        }
    };
    let mut targets: BTreeMap<String, Target> = BTreeMap::new();
    let mut next_look = Instant::now();
    loop {
        let parsed = Config::parse(&config.lock().await.text).ok();
        if let Some(parsed) = &parsed {
            reconcile(&store, &mut targets, &mut stored, parsed).await;
        }
        let looking = Instant::now() >= next_look;
        if looking {
            next_look = Instant::now() + setup.timing.check;
        }
        if let Some(parsed) = parsed.filter(|_| !targets.is_empty()) {
            step(&store, &http, &setup, &mut targets, &parsed, looking).await;
        }
        let wake = targets
            .values()
            .filter_map(|target| target.due(&setup.timing))
            .chain([next_look])
            .min()
            .unwrap_or(next_look);
        tokio::select! {
            () = tokio::time::sleep_until(wake) => {}
            closed = durable(&mut live) => if closed { return },
            () = replaced.notified() => {}
            _ = stopping.wait_for(|stopping| *stopping) => return,
        }
    }
}

/// Waits for the next durable event; `true` once the store is gone. A client that lagged reads the store anyway.
async fn durable(live: &mut broadcast::Receiver<Arc<EventEnvelope>>) -> bool {
    loop {
        match live.recv().await {
            Ok(envelope) if envelope.id.is_none() => {}
            Ok(_) | Err(RecvError::Lagged(_)) => return false,
            Err(RecvError::Closed) => return true,
        }
    }
}

/// Follows `[notify]`'s targets: a new one resumes where the history says it got to, or starts now; a removed one is
/// forgotten.
async fn reconcile(
    store: &Store,
    targets: &mut BTreeMap<String, Target>,
    stored: &mut BTreeMap<String, BTreeMap<String, u64>>,
    config: &Config,
) {
    let names: BTreeSet<&str> = config
        .notify
        .targets
        .iter()
        .map(|target| target.name.as_str())
        .collect();
    let removed = targets.keys().any(|name| !names.contains(name.as_str()))
        || stored.keys().any(|name| !names.contains(name.as_str()));
    if removed {
        targets.retain(|name, _| names.contains(name.as_str()));
        stored.retain(|name, _| names.contains(name.as_str()));
        if !store
            .notify_keep(names.iter().map(|name| (*name).to_owned()).collect())
            .await
        {
            warn!("notify: cannot forget the removed targets in the history");
        }
    }
    for wanted in &config.notify.targets {
        if let Some(target) = targets.get_mut(&wanted.name) {
            target.config = wanted.clone();
            continue;
        }
        let Some(target) = start(store, stored.get(&wanted.name).cloned(), wanted, config).await
        else {
            continue;
        };
        stored.remove(&wanted.name);
        targets.insert(wanted.name.clone(), target);
    }
}

/// A target as the history left it, or, for one it has nothing of, starting now: at the newest event, with the
/// insights that hold now told and today's digest sent, so adding a target replays nothing.
async fn start(
    store: &Store,
    kept: Option<BTreeMap<String, u64>>,
    config: &NotifyTarget,
    daemon: &Config,
) -> Option<Target> {
    let now = now() * 1000;
    let mut kept = match kept.filter(|kept| kept.contains_key("event")) {
        Some(kept) => kept,
        None => begin(store, config, daemon, now).await?,
    };
    let cursor = kept.remove("event").unwrap_or(0);
    let last_digest = kept.remove("digest").unwrap_or(now);
    let told = kept
        .into_iter()
        .filter_map(|(key, seen)| Some((key.strip_prefix("insight:")?.to_owned(), seen)))
        .collect();
    Some(Target {
        config: config.clone(),
        cursor,
        stored: cursor,
        failed: Kept::default(),
        lost: Kept::default(),
        runners: Kept::default(),
        insights: Vec::new(),
        present: None,
        told,
        looked: false,
        digest: None,
        last_digest,
        since: None,
        sent: None,
        retry_at: None,
        failures: 0,
    })
}

/// Records a target the history has nothing of as starting at `now` (ms): at the newest event, with the insights
/// that hold now told and today's digest sent. What it recorded, or `None` if it could not.
async fn begin(
    store: &Store,
    config: &NotifyTarget,
    daemon: &Config,
    now: u64,
) -> Option<BTreeMap<String, u64>> {
    let event = store
        .last_event()
        .await
        .map_err(|store_error| {
            warn!(
                "notify {}: cannot read the history: {store_error}",
                config.name
            );
        })
        .ok()?;
    let mut set = vec![("event".to_owned(), event), ("digest".to_owned(), now)];
    if let Ok(runs) = insight_runs_of(store, runnerd_core::api::INSIGHT_DAYS, now).await {
        set.extend(
            warnings(&runs, daemon, now)
                .into_iter()
                .map(|insight| (format!("insight:{}", insight.key), now)),
        );
    }
    if !store
        .notify_record(&config.name, set.clone(), Vec::new())
        .await
    {
        warn!("notify {}: cannot record where it starts", config.name);
        return None;
    }
    info!("notify {}: starts with the events from now on", config.name);
    Some(set.into_iter().collect())
}

fn warnings(history: &insights::History, config: &Config, now: u64) -> Vec<Insight> {
    insights::insights(history, config, runnerd_core::api::INSIGHT_DAYS, now)
        .insights
        .into_iter()
        .filter(|insight| insight.severity == Severity::Warning)
        .collect()
}

/// Reads the events after each target's cursor, looks at insights and the digest when `looking`, and sends what is
/// due.
async fn step(
    store: &Store,
    http: &reqwest::Client,
    setup: &Setup,
    targets: &mut BTreeMap<String, Target>,
    config: &Config,
    looking: bool,
) {
    let now_ms = now() * 1000;
    // Read once for every target that wants them.
    let wants = |event| targets.values().any(|target| target.wants(event));
    let runs = if looking && (wants(NotifyEvent::Insights) || wants(NotifyEvent::Digest)) {
        insight_runs_of(store, runnerd_core::api::INSIGHT_DAYS, now_ms)
            .await
            .map_err(|store_error| warn!("notify: cannot read the history: {store_error}"))
            .ok()
    } else {
        None
    };
    let offset = if looking && wants(NotifyEvent::Digest) {
        let read = setup.offset;
        tokio::task::spawn_blocking(read).await.ok().flatten()
    } else {
        None
    };
    let warned = runs.as_ref().map(|runs| warnings(runs, config, now_ms));
    for target in targets.values_mut() {
        if target.retry_at.is_some_and(|at| at > Instant::now()) {
            continue;
        }
        if !read_events(store, target).await {
            let wait = target.back_off(&setup.timing);
            warn!(
                "notify {}: reading the history again in {}s",
                target.config.name,
                wait.as_secs()
            );
            continue;
        }
        if let Some(warned) = warned
            .as_ref()
            .filter(|_| target.wants(NotifyEvent::Insights))
        {
            target.look(warned, now_ms);
        }
        if let Some(runs) = &runs
            && let Some(offset) = offset
            && target.wants(NotifyEvent::Digest)
            && target.digest.is_none()
            && digest_due(
                now_ms / 1000,
                offset,
                config.notify.digest_at,
                target.last_digest / 1000,
            )
        {
            let mut digest = insights::digest(runs, config, now_ms);
            digest.savings = savings_of(store, config, now_ms)
                .await
                .map_err(|store_error| {
                    warn!("notify: cannot price the month's jobs: {store_error}");
                })
                .ok()
                .map(|savings| savings.month);
            target.digest = Some(digest);
        }
        send_due(store, http, &setup.timing, target).await;
    }
}

/// Takes in the stored events after the target's cursor; `false` when the history cannot be read.
async fn read_events(store: &Store, target: &mut Target) -> bool {
    loop {
        let batch = match store.events_after(target.cursor, 500).await {
            Ok(batch) => batch,
            Err(store_error) => {
                warn!(
                    "notify {}: cannot read events: {store_error}",
                    target.config.name
                );
                return false;
            }
        };
        let Some(last) = batch.last().and_then(|envelope| envelope.id) else {
            return true;
        };
        for envelope in batch {
            target.take(envelope.event);
        }
        target.cursor = last;
    }
}

/// Sends the target's message and its digest if they are due, and records what was.
async fn send_due(store: &Store, http: &reqwest::Client, timing: &Timing, target: &mut Target) {
    let ready = target.due(timing).is_some_and(|due| due <= Instant::now());
    if !target.waiting() {
        // Nothing to send: the cursor moves past the events not wanted, and the insights still held are seen now.
        let (mut set, remove) = target.told_now();
        if target.cursor != target.stored {
            set.push(("event".to_owned(), target.cursor));
        }
        if (!set.is_empty() || !remove.is_empty())
            && store.notify_record(&target.config.name, set, remove).await
        {
            target.stored = target.cursor;
        }
    } else if ready {
        let notification = message(target);
        match deliver(http, &target.config, &notification).await {
            Ok(()) => {
                let (mut set, remove) = target.told_now();
                set.push(("event".to_owned(), target.cursor));
                if store.notify_record(&target.config.name, set, remove).await {
                    target.stored = target.cursor;
                } else {
                    warn!(
                        "notify {}: sent, but the history did not record it, so a restart may send it again",
                        target.config.name
                    );
                }
                target.failed = Kept::default();
                target.lost = Kept::default();
                target.runners = Kept::default();
                target.insights.clear();
                target.since = None;
                target.delivered();
                // A digest due too waits for the gap.
                return;
            }
            Err(why) => return target.failed_to_send(timing, &why),
        }
    }
    if !ready {
        return;
    }
    if let Some(digest) = target.digest.clone() {
        // The day it is for is the day it was made: one delivered after midnight still counts for the day before.
        let made = digest.until * 1000;
        match deliver(http, &target.config, &digest_message(digest)).await {
            Ok(()) => {
                if !store
                    .notify_record(
                        &target.config.name,
                        vec![("digest".to_owned(), made)],
                        Vec::new(),
                    )
                    .await
                {
                    warn!(
                        "notify {}: sent the digest, but the history did not record it",
                        target.config.name
                    );
                }
                target.last_digest = made;
                target.digest = None;
                target.delivered();
            }
            Err(why) => target.failed_to_send(timing, &why),
        }
    }
}

/// `repository: workflow / job`.
fn job_line(job: &JobSummary) -> String {
    format!("{}: {}", job.repository, job.title())
}

fn on_branch(job: &JobSummary) -> String {
    job.branch
        .as_ref()
        .map_or_else(String::new, |branch| format!(" on {branch}"))
}

/// Up to five lines of `count`, and how many more.
fn lines(all: impl Iterator<Item = String>, count: usize) -> Vec<String> {
    let mut lines: Vec<String> = all.take(5).collect();
    if count > 5 {
        lines.push(format!("and {} more", count - 5));
    }
    lines
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// The title of the target's message: what it is about, or how many of each.
fn title(target: &Target) -> String {
    let (failed, lost, runners, found) = (
        &target.failed,
        &target.lost,
        &target.runners,
        &target.insights,
    );
    match (
        (failed.count, failed.first.first()),
        (lost.count, lost.first.first()),
        (runners.count, runners.first.first()),
        (found.len(), found.first()),
    ) {
        ((1, Some(job)), (0, _), (0, _), (0, _)) => format!("{} failed", job_line(job)),
        ((jobs, _), (0, _), (0, _), (0, _)) => format!("{jobs} jobs failed"),
        ((0, _), (1, Some(job)), (0, _), (0, _)) => format!("{} lost its runner", job_line(job)),
        ((0, _), (jobs, _), (0, _), (0, _)) => format!("{jobs} jobs lost their runner"),
        ((0, _), (0, _), (1, Some(failure)), (0, _)) => {
            format!("{}: no runner starts", failure.repository)
        }
        ((0, _), (0, _), (failures, _), (0, _)) => {
            format!("Runners cannot start for {failures} repositories")
        }
        ((0, _), (0, _), (0, _), (1, Some(insight))) => insight_title(insight),
        ((0, _), (0, _), (0, _), (insights, _)) => format!("{insights} new insights"),
        _ => [
            (!failed.is_empty()).then(|| plural(failed.count, "job failed", "jobs failed")),
            (!lost.is_empty()).then(|| plural(lost.count, "job lost", "jobs lost")),
            (!runners.is_empty()).then(|| plural(runners.count, "runner failed", "runners failed")),
            (!found.is_empty()).then(|| plural(found.len(), "new insight", "new insights")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", "),
    }
}

/// What the target is sent for its jobs, runners and insights: one message, however many.
fn message(target: &Target) -> Notification {
    let (failed, lost, runners, found) = (
        &target.failed,
        &target.lost,
        &target.runners,
        &target.insights,
    );
    let title = title(target);
    let mut body = Vec::new();
    body.extend(lines(
        failed.first.iter().map(|job| {
            let how = if job.conclusion.as_deref() == Some("timed_out") {
                "timed out"
            } else {
                "failed"
            };
            format!("{} {how}{}", job_line(job), on_branch(job))
        }),
        failed.count,
    ));
    body.extend(lines(
        lost.first
            .iter()
            .map(|job| format!("{} lost its runner{}", job_line(job), on_branch(job))),
        lost.count,
    ));
    body.extend(lines(
        runners.first.iter().map(|failure| {
            format!(
                "{} ({}): {}; no runner for {}s",
                failure.repository, failure.profile, failure.error, failure.held_for
            )
        }),
        runners.count,
    ));
    body.extend(lines(
        found.iter().map(|insight| insight.sentence.clone()),
        found.len(),
    ));
    let event = if !failed.is_empty() {
        NotifyEvent::Failed
    } else if !runners.is_empty() {
        NotifyEvent::Runner
    } else if !lost.is_empty() {
        NotifyEvent::Lost
    } else {
        NotifyEvent::Insights
    };
    let url = failed
        .first
        .iter()
        .chain(&lost.first)
        .find_map(|job| job.url.clone())
        .or_else(|| {
            found
                .iter()
                .flat_map(|insight| &insight.runs)
                .find_map(|run| run.url.clone())
        });
    Notification {
        event,
        title,
        message: body.join("\n"),
        url,
        priority: if failed.is_empty() && runners.is_empty() {
            Priority::Default
        } else {
            Priority::High
        },
        jobs: failed.first.iter().chain(&lost.first).cloned().collect(),
        runners: runners.first.clone(),
        insights: found.clone(),
        digest: None,
    }
}

fn insight_title(insight: &Insight) -> String {
    let subject = match &insight.job {
        Some(job) => format!(
            "{}: {}",
            insight.repository,
            job_title(insight.workflow.as_deref(), job, None)
        ),
        None => insight.repository.clone(),
    };
    let what = match insight.kind {
        runnerd_core::api::InsightKind::Slower { .. } => "got slower",
        runnerd_core::api::InsightKind::Flaky { .. } => "is flaky",
        runnerd_core::api::InsightKind::NearCap { .. } => "nears its memory cap",
        runnerd_core::api::InsightKind::ColdCache { .. } => "misses the build cache",
        runnerd_core::api::InsightKind::Throttled { .. } => "was slowed by memory pressure",
        runnerd_core::api::InsightKind::CpuHeld { .. } => "is held back by its CPU cap",
        runnerd_core::api::InsightKind::Waits { .. } => "waits longer",
        runnerd_core::api::InsightKind::Unknown => "has an insight",
    };
    format!("{subject} {what}")
}

fn digest_message(digest: Digest) -> Notification {
    let mut body = vec![format!("{}.", digest.summary_line())];
    body.extend(lines(
        digest.new.iter().map(|insight| insight.sentence.clone()),
        digest.new.len(),
    ));
    if !digest.waits.is_empty() {
        body.push("Longest waits:".to_owned());
        body.extend(digest.waits.iter().map(runnerd_core::api::LongWait::line));
    }
    if let Some(savings) = &digest.savings {
        body.push(format!("{}.", savings.headline()));
    }
    Notification {
        event: NotifyEvent::Digest,
        title: "CI in the last 24 hours".to_owned(),
        message: body.join("\n"),
        url: None,
        priority: Priority::Low,
        jobs: Vec::new(),
        runners: Vec::new(),
        insights: Vec::new(),
        digest: Some(digest),
    }
}

/// `POST /v1/notify/test?target=`: sends a test message to every target, or to the one named, and answers what each
/// did.
pub(super) async fn test(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Query(query): Query<NotifyTestQuery>,
) -> Response {
    let text = state.config.lock().await.text.clone();
    let notify = match Config::parse(&text) {
        Ok(config) => config.notify,
        Err(parse_error) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, &parse_error.to_string());
        }
    };
    let Notify { targets, .. } = notify;
    let targets: Vec<NotifyTarget> = targets
        .into_iter()
        .filter(|target| {
            query
                .target
                .as_ref()
                .is_none_or(|name| *name == target.name)
        })
        .collect();
    if targets.is_empty() {
        let message = match &query.target {
            Some(name) => format!("[notify] has no target named {name:?}"),
            None => "[notify] has no targets; add one to runnerd.toml".to_owned(),
        };
        return error(StatusCode::NOT_FOUND, &message);
    }
    let http = match client(TIMING.timeout) {
        Ok(http) => http,
        Err(why) => return error(StatusCode::INTERNAL_SERVER_ERROR, &why),
    };
    info!("notify: test message requested by {principal:?}");
    // All at once, so slow targets together still answer within the client's wait.
    let deliveries = futures_util::future::join_all(targets.iter().map(|target| async {
        let error = deliver(&http, target, &test_message(&target.name))
            .await
            .err();
        if let Some(why) = &error {
            warn!("notify {}: test not delivered: {why}", target.name);
        }
        Delivery {
            target: target.name.clone(),
            error,
        }
    }))
    .await;
    Json(NotifyTest {
        targets: deliveries,
    })
    .into_response()
}

/// What `runnerctl notify test` sends the target named `name`.
fn test_message(name: &str) -> Notification {
    Notification {
        event: NotifyEvent::Test,
        title: "runnerd can reach you".to_owned(),
        message: format!("A test from runnerd: notify target {name:?} works."),
        url: None,
        priority: Priority::Default,
        jobs: Vec::new(),
        runners: Vec::new(),
        insights: Vec::new(),
        digest: None,
    }
}

#[cfg(test)]
mod tests;
