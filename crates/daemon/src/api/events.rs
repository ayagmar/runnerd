//! `GET /v1/events`: the event stream as server-sent events. With `Last-Event-ID`, the durable events stored after
//! that ID come first, then live ones; an event read from the store is never sent again live. A client that falls
//! too far behind is disconnected and resumes with its last ID. Streams end when the server stops.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{self, KeepAlive, Sse};
use futures_util::Stream;
use futures_util::stream;
use runnerd_core::api::{EventEnvelope, EventQuery};
use runnerd_core::store::Store;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use super::{AppState, Authenticated};

/// Stored events per read while catching up.
const REPLAY_BATCH: u64 = 500;

struct Events {
    store: Store,
    query: EventQuery,
    /// The newest durable event sent; `None` until one was, when no `Last-Event-ID` was given.
    sent: Option<u64>,
    replaying: bool,
    pending: std::vec::IntoIter<EventEnvelope>,
    live: broadcast::Receiver<Arc<EventEnvelope>>,
    stopping: watch::Receiver<bool>,
}

fn sse_event(envelope: &EventEnvelope) -> sse::Event {
    let event = sse::Event::default().event(envelope.event.kind());
    let event = match envelope.id {
        Some(id) => event.id(id.to_string()),
        None => event,
    };
    match serde_json::to_string(envelope) {
        Ok(data) => event.data(data),
        Err(error) => event.comment(format!("cannot encode the event: {error}")),
    }
}

impl Events {
    async fn next(&mut self) -> Option<EventEnvelope> {
        loop {
            if let Some(envelope) = self.pending.next() {
                self.sent = envelope.id.or(self.sent);
                if self.query.matches(&envelope.event) {
                    return Some(envelope);
                }
                continue;
            }
            if self.replaying {
                match self
                    .store
                    .events_after(self.sent.unwrap_or(0), REPLAY_BATCH)
                    .await
                {
                    Ok(batch) if !batch.is_empty() => {
                        self.pending = batch.into_iter();
                        continue;
                    }
                    Ok(_) => self.replaying = false,
                    Err(error) => {
                        warn!("cannot read stored events: {error}");
                        return None;
                    }
                }
            }
            let received = tokio::select! {
                received = self.live.recv() => received,
                _ = self.stopping.wait_for(|stopping| *stopping) => return None,
            };
            match received {
                Ok(envelope) => {
                    // Already sent from the store.
                    if envelope
                        .id
                        .is_some_and(|id| self.sent.is_some_and(|sent| id <= sent))
                    {
                        continue;
                    }
                    self.sent = envelope.id.or(self.sent);
                    if self.query.matches(&envelope.event) {
                        return Some((*envelope).clone());
                    }
                }
                // Too far behind: the client resumes from its last ID.
                Err(RecvError::Lagged(_) | RecvError::Closed) => return None,
            }
        }
    }
}

pub(super) async fn events(
    _: Authenticated,
    State(state): State<AppState>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let last = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let events = Events {
        // Subscribed before reading the store, so nothing stored meanwhile is missed.
        live: state.handle.store.subscribe(),
        store: state.handle.store.clone(),
        query,
        sent: last,
        replaying: last.is_some(),
        pending: Vec::new().into_iter(),
        stopping: state.stopping.clone(),
    };
    let stream = stream::unfold(events, |mut events| async move {
        let envelope = events.next().await?;
        Some((Ok(sse_event(&envelope)), events))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
