//! The job history routes: jobs, their samples and logs, the slice's samples, stats and cache sizes, read from the
//! store. Every handler takes the authenticated principal. Reads only; a log with `follow` streams until its job's
//! record is final or the server stops.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use runnerd_core::api::{
    CacheHistory, CacheHistoryQuery, Caches, Event, HostSampleQuery, HostSamples, JobQuery,
    LogQuery, StatsQuery,
};
use runnerd_core::observe::metrics::SAMPLE_INTERVAL;
use runnerd_core::store::{Store, StoreError};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

use super::{AppState, Authenticated, error};

/// Lines per read while a log streams.
const LOG_BATCH: u64 = 2000;

fn now() -> u64 {
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
    let since = query.since.unwrap_or_else(|| now().saturating_sub(3600));
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

pub(super) async fn caches(_: Authenticated, State(state): State<AppState>) -> Response {
    match state.handle.store.cache_sizes_now().await {
        Ok((measured_at, sizes)) => {
            let volumes = state
                .handle
                .caches
                .iter()
                .cloned()
                .map(|mut volume| {
                    volume.bytes = sizes
                        .iter()
                        .find(|(name, _)| *name == volume.name)
                        .map_or(0, |(_, bytes)| *bytes);
                    volume
                })
                .collect();
            Json(Caches {
                measured_at,
                volumes,
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
