//! The job history routes: jobs, their samples and logs, the slice's samples, stats, sizing advice, insights, what the
//! jobs' minutes would have cost on GitHub's runners and cache sizes,
//! read from the store. Every handler takes the authenticated principal. Reads only; a log with `follow` streams
//! until its job's record is final or the server stops.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use runnerd_core::api::{
    ActivityQuery, BuildCacheStats, CacheHistory, CacheHistoryQuery, CacheKind, Caches, Event,
    HostSampleQuery, HostSamples, INSIGHT_DAYS, INSIGHT_DAYS_MAX, InsightsQuery, JobQuery,
    LogQuery, Savings, Sizing, SizingQuery, StatsQuery, reads_since,
};
use runnerd_core::config::Config;
use runnerd_core::insights::{self, History};
use runnerd_core::observe::metrics::SAMPLE_INTERVAL;
use runnerd_core::sizing::memory::HISTORY;
use runnerd_core::sizing::{Window, advise};
use runnerd_core::store::{Store, StoreError};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

use super::{AppState, Authenticated, error};

/// Lines per read while a log streams.
const LOG_BATCH: u64 = 2000;

/// How far back `/v1/host/samples` goes: 43,200 samples, about 2 MB of JSON. Seven days of them were 15 MB.
const HOST_SAMPLES_WINDOW: u64 = 24 * 3600;

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn failed(store_error: &StoreError) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, &store_error.to_string())
}

fn no_job(id: u64) -> Response {
    error(
        StatusCode::NOT_FOUND,
        &format!("no job {id} in the history"),
    )
}

pub(super) async fn jobs(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<JobQuery>,
) -> Response {
    match state.handle.store.jobs(query).await {
        Ok(page) => Json(page).into_response(),
        Err(store_error) => failed(&store_error),
    }
}

pub(super) async fn job(
    _: Authenticated,
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Response {
    match state.handle.store.job(id).await {
        Ok(Some(job)) => Json(job).into_response(),
        Ok(None) => no_job(id),
        Err(store_error) => failed(&store_error),
    }
}

pub(super) async fn samples(
    _: Authenticated,
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Response {
    match state.handle.store.job_samples(id, SAMPLE_INTERVAL).await {
        Ok(Some(series)) => Json(series).into_response(),
        Ok(None) => no_job(id),
        Err(store_error) => failed(&store_error),
    }
}

pub(super) async fn host_samples(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<HostSampleQuery>,
) -> Response {
    let now = now();
    let since = query
        .since
        .unwrap_or_else(|| now.saturating_sub(3600))
        .max(now.saturating_sub(HOST_SAMPLES_WINDOW));
    match state.handle.store.host_samples(since).await {
        Ok(samples) => Json(HostSamples {
            interval: SAMPLE_INTERVAL.as_secs(),
            samples,
        })
        .into_response(),
        Err(store_error) => failed(&store_error),
    }
}

pub(super) async fn stats(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<StatsQuery>,
) -> Response {
    match state.handle.store.stats(query, now()).await {
        Ok(rows) => Json(rows).into_response(),
        Err(store_error) => failed(&store_error),
    }
}

pub(super) async fn activity(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<ActivityQuery>,
) -> Response {
    let now = now();
    let since = query.since.unwrap_or(now.saturating_sub(24 * 3600));
    match state.handle.store.activity(since, now).await {
        Ok(activity) => Json(activity).into_response(),
        Err(store_error) => failed(&store_error),
    }
}

pub(super) async fn sizing(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<SizingQuery>,
) -> Response {
    let window = Window::new(query.since, now());
    match sizing_now(&state, window).await {
        Ok(sizing) => Json(sizing).into_response(),
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// The advice for the config the daemon runs with, from the history in `window`, against the slice's `MemoryMax`
/// read now, as a config check reads it: one raised since the daemon started counts without a restart.
pub(super) async fn sizing_now(state: &AppState, window: Window) -> Result<Sizing, String> {
    // The windows always read the last 30 days; `since` narrows only the waits, the busy share and the starts.
    let mut history = state
        .handle
        .store
        .sizing(window.since.min(window.now.saturating_sub(HISTORY)))
        .await
        .map_err(|store_error| store_error.to_string())?;
    let auto = super::auto::last_changes(state)
        .await
        .map_err(|store_error| store_error.to_string())?;
    super::auto::recorded(state, &mut history, &auto, window)
        .await
        .map_err(|store_error| store_error.to_string())?;
    let text = state.config.lock().await.text.clone();
    let config = Config::parse(&text).map_err(|parse_error| parse_error.to_string())?;
    let (read, slice) = (state.memory_max, config.host.slice.clone());
    let memory_max = tokio::task::spawn_blocking(move || read(&slice))
        .await
        .map_err(|join_error| join_error.to_string())?
        .map_err(|read_error| {
            format!(
                "cannot read {}'s MemoryMax: {read_error}",
                config.host.slice
            )
        })?;
    let changes = state
        .handle
        .store
        .sizing_changes(window.since)
        .await
        .map_err(|store_error| store_error.to_string())?;
    let memory_max = memory_max.map(|max| max.0);
    // A forecast: the advice stands without it.
    let auto_plan = super::auto::plan(state, &config, memory_max)
        .await
        .unwrap_or_else(|message| {
            tracing::warn!("auto sizing: cannot work out its next look: {message}");
            None
        });
    Ok(Sizing {
        changes,
        auto_plan,
        ..advise(&config, memory_max, &history, window)
    })
}

pub(super) async fn insights(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<InsightsQuery>,
) -> Response {
    let days = query
        .days
        .unwrap_or(INSIGHT_DAYS)
        .clamp(1, INSIGHT_DAYS_MAX);
    let now = now() * 1000;
    match insight_runs(&state, days, now).await {
        Ok((runs, config)) => Json(insights::insights(&runs, &config, days, now)).into_response(),
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

pub(super) async fn digest(_: Authenticated, State(state): State<AppState>) -> Response {
    let now = now() * 1000;
    match insight_runs(&state, INSIGHT_DAYS, now).await {
        Ok((runs, config)) => {
            let mut digest = insights::digest(&runs, &config, now);
            digest.savings = savings_of(&state.handle.store, &config, now)
                .await
                .ok()
                .map(|savings| savings.month);
            Json(digest).into_response()
        }
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// `GET /v1/savings`: what the jobs' minutes this month and in the last 30 days would have cost on GitHub's runners,
/// priced at the config running now (ADR 0072).
pub(super) async fn savings(_: Authenticated, State(state): State<AppState>) -> Response {
    let text = state.config.lock().await.text.clone();
    let config = match Config::parse(&text) {
        Ok(config) => config,
        Err(parse_error) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, &parse_error.to_string());
        }
    };
    match savings_of(&state.handle.store, &config, now() * 1000).await {
        Ok(savings) => Json(savings).into_response(),
        Err(store_error) => failed(&store_error),
    }
}

/// What the history's job minutes would have cost on GitHub's runners at `now` (ms), priced at `config`.
pub(super) async fn savings_of(
    store: &Store,
    config: &Config,
    now: u64,
) -> Result<Savings, StoreError> {
    let history = store.pricing(reads_since(now)).await?;
    Ok(Savings::of(&history, &config.pricing(), now))
}

/// What insights of `days` ending at `now` (ms) read: the jobs of those days, of the 14 the waits compare and of
/// the day before, for the digest; and the config the daemon runs with.
pub(super) async fn insight_runs(
    state: &AppState,
    days: u32,
    now: u64,
) -> Result<(History, Config), String> {
    let runs = insight_runs_of(&state.handle.store, days, now)
        .await
        .map_err(|store_error| store_error.to_string())?;
    let text = state.config.lock().await.text.clone();
    let config = Config::parse(&text).map_err(|parse_error| parse_error.to_string())?;
    Ok((runs, config))
}

/// The jobs insights of `days` ending at `now` (ms) read.
pub(super) async fn insight_runs_of(
    store: &Store,
    days: u32,
    now: u64,
) -> Result<History, StoreError> {
    const DAY: u64 = 24 * 3600 * 1000;
    let back = u64::from(days.max(14)) + 1;
    store.insights(now.saturating_sub(back * DAY)).await
}

pub(super) async fn caches(_: Authenticated, State(state): State<AppState>) -> Response {
    const WEEK: u64 = 7 * 24 * 3600 * 1000;
    let build = match &state.handle.build_cache {
        Some(cache) => {
            let limit = state
                .handle
                .caches
                .borrow()
                .iter()
                .find(|volume| volume.kind == CacheKind::Build)
                .and_then(|volume| volume.limit);
            match limit {
                Some(limit) => {
                    let since = (now() * 1000).saturating_sub(WEEK);
                    match state.handle.store.build_cache_since(since).await {
                        Ok(week) => {
                            let size = cache.size();
                            Some(BuildCacheStats {
                                bytes: size.bytes,
                                entries: size.entries,
                                limit,
                                week,
                            })
                        }
                        Err(store_error) => return failed(&store_error),
                    }
                }
                // Off in the config.
                None => None,
            }
        }
        None => None,
    };
    match state.handle.store.cache_sizes_now().await {
        Ok(measured) => {
            let volumes = state
                .handle
                .caches
                .borrow()
                .iter()
                .cloned()
                .map(|mut volume| {
                    volume.bytes = match (&volume.kind, build) {
                        // Its size now, which the daemon keeps without measuring.
                        (CacheKind::Build, Some(build)) => build.bytes,
                        _ => measured.bytes.get(&volume.name).copied().unwrap_or(0),
                    };
                    volume
                })
                .collect();
            Json(Caches {
                measured_at: measured.at,
                volumes,
                build,
            })
            .into_response()
        }
        Err(store_error) => failed(&store_error),
    }
}

pub(super) async fn cache_history(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<CacheHistoryQuery>,
) -> Response {
    let since = query
        .since
        .unwrap_or_else(|| now().saturating_sub(30 * 24 * 3600));
    match state.handle.store.cache_history(since).await {
        Ok(sizes) => Json(CacheHistory { sizes }).into_response(),
        Err(store_error) => failed(&store_error),
    }
}

/// Where a log stream is.
enum Phase {
    /// Reading stored lines; then, if `last`, the stream ends, else it waits for new lines.
    Stored {
        last: bool,
    },
    /// Waiting for `log.line` events.
    Live,
    Done,
}

struct LogStream {
    store: Store,
    job: u64,
    next: u64,
    follow: bool,
    phase: Phase,
    events: broadcast::Receiver<std::sync::Arc<runnerd_core::api::EventEnvelope>>,
    stopping: watch::Receiver<bool>,
}

fn lines(lines: &[String]) -> Bytes {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    Bytes::from(text)
}

impl LogStream {
    /// The next part of the body; `None` at the end.
    async fn next(&mut self) -> Option<Result<Bytes, StoreError>> {
        loop {
            match self.phase {
                Phase::Done => return None,
                Phase::Stored { last } => {
                    let batch = match self.store.log_lines(self.job, self.next, LOG_BATCH).await {
                        Ok(batch) => batch,
                        Err(store_error) => {
                            self.phase = Phase::Done;
                            return Some(Err(store_error));
                        }
                    };
                    if let Some((line, _)) = batch.last() {
                        self.next = line + 1;
                        let text: Vec<String> = batch.into_iter().map(|(_, text)| text).collect();
                        return Some(Ok(lines(&text)));
                    }
                    self.phase = if last || !self.follow {
                        Phase::Done
                    } else {
                        match self.store.job(self.job).await {
                            // Closed before or while it was read: everything is stored.
                            Ok(Some(job)) if job.summary.state.is_final() => {
                                Phase::Stored { last: true }
                            }
                            Ok(_) => Phase::Live,
                            Err(store_error) => {
                                self.phase = Phase::Done;
                                return Some(Err(store_error));
                            }
                        }
                    };
                }
                Phase::Live => {
                    let received = tokio::select! {
                        received = self.events.recv() => received,
                        _ = self.stopping.wait_for(|stopping| *stopping) => return None,
                    };
                    match received {
                        Ok(envelope) => match &envelope.event {
                            Event::LogLine { job, line, text } if *job == self.job => {
                                if *line == self.next {
                                    self.next += 1;
                                    return Some(Ok(lines(std::slice::from_ref(text))));
                                }
                                if *line > self.next {
                                    self.phase = Phase::Stored { last: false };
                                }
                            }
                            Event::JobFinished { job } if job.id == self.job => {
                                self.phase = Phase::Stored { last: true };
                            }
                            _ => {}
                        },
                        // Fell behind: the store has every line.
                        Err(RecvError::Lagged(_)) => self.phase = Phase::Stored { last: false },
                        Err(RecvError::Closed) => self.phase = Phase::Done,
                    }
                }
            }
        }
    }
}

pub(super) async fn logs(
    _: Authenticated,
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Query(query): Query<LogQuery>,
) -> Response {
    let store = state.handle.store.clone();
    // Subscribed before the first read, so no line stored meanwhile is missed.
    let events = store.subscribe();
    match store.job(id).await {
        Ok(Some(_)) => {}
        Ok(None) => return no_job(id),
        Err(store_error) => return failed(&store_error),
    }
    let log = LogStream {
        store,
        job: id,
        next: query.from.unwrap_or(0),
        follow: query.follow,
        phase: Phase::Stored { last: false },
        events,
        stopping: state.stopping.clone(),
    };
    let body = Body::from_stream(stream::unfold(log, |mut log| async move {
        log.next().await.map(|part| (part, log))
    }));
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}
