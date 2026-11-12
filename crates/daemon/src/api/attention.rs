//! `GET /v1/attention`: what needs attention now, kept by one worker that owns the snapshot (ADR 0132). The worker
//! words nothing itself: `api::attention_items` decides every item from the engine's status, the doctor's last run
//! (`doctor_cache`) and the insights. It looks again whenever the status, the doctor's run or the config changes, on
//! each durable event, and every `Timing::every`, so a condition that comes or goes with time alone shows; it reads
//! the insights at most once a `Timing::insights`, and again at once after the config is replaced. The revision moves
//! only when the content does, also when one item replaces another, and each move is sent as `attention.changed`.
//!
//! It must not change the engine, the store or the config: it reads them and publishes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use runnerd_core::api::{
    self, Attention, Doctor, DoctorState, Event, EventEnvelope, INSIGHT_DAYS, Insights,
};
use runnerd_core::insights;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use super::{AppState, Authenticated};

/// How often the worker looks with nothing changing, and how old the insights it reads may be.
#[derive(Clone, Copy)]
pub(crate) struct Timing {
    pub every: Duration,
    pub insights: Duration,
}

/// The daemon's own: a look every 30 seconds, insights at most a minute old.
pub(crate) const TIMING: Timing = Timing {
    every: Duration::from_secs(30),
    insights: Duration::from_mins(1),
};

/// The snapshot and its revision. Only `update` writes it. Nothing of it survives a restart: the revision starts again
/// at the daemon's start time in milliseconds, past any the daemon before reached unless that changed more than once a
/// millisecond on average, so a client never mistakes a new snapshot for the one it holds.
struct Snapshot {
    shown: Attention,
}

impl Snapshot {
    /// Takes `items` in place of what it shows: the new revision when they differ, `None` when they say the same.
    fn update(&mut self, items: Vec<api::AttentionItem>) -> Option<u64> {
        let next = Attention::new(self.shown.revision + 1, items);
        if next.same_content(&self.shown) {
            return None;
        }
        self.shown = next;
        Some(self.shown.revision)
    }
}

/// The insights the worker read last.
struct Read {
    at: Instant,
    /// `None` when the read failed: no insight is listed then.
    insights: Option<Insights>,
}

/// What needs attention before the worker's first look: the status as it is and no doctor run, so never an all clear.
pub(super) fn first(status: &api::Status, start: u64) -> Attention {
    let doctor = Doctor {
        checks: Vec::new(),
        state: Some(DoctorState::NotRun),
    };
    Attention::new(
        start,
        api::attention_items(status, &doctor, None, super::history::now()),
    )
}

/// `GET /v1/attention`.
pub(super) async fn attention(_: Authenticated, State(state): State<AppState>) -> Json<Attention> {
    Json(state.attention.borrow().clone())
}

/// Keeps `published` current until the server stops.
pub(super) async fn run(state: AppState, published: watch::Sender<Attention>, timing: Timing) {
    let mut snapshot = Snapshot {
        shown: published.borrow().clone(),
    };
    let mut status = state.handle.status.clone();
    let mut doctor = state.doctors.subscribe();
    let mut replaced = state.config_revision.subscribe();
    let mut live = state.handle.store.subscribe();
    let mut stopping = state.stopping.clone();
    let mut read: Option<Read> = None;
    loop {
        let config_replaced = replaced.has_changed().unwrap_or(false);
        let revision = replaced.borrow_and_update().clone();
        if config_replaced
            || read
                .as_ref()
                .is_none_or(|read| read.at.elapsed() >= timing.insights)
        {
            read = Some(Read {
                at: Instant::now(),
                insights: read_insights(&state).await,
            });
        }
        let now = super::history::now();
        let report = doctor
            .borrow_and_update()
            .report(&revision, now, state.doctors.timing());
        let items = api::attention_items(
            &status.borrow_and_update(),
            &report,
            read.as_ref().and_then(|read| read.insights.as_ref()),
            now,
        );
        if let Some(revision) = snapshot.update(items) {
            published.send_replace(snapshot.shown.clone());
            state
                .handle
                .store
                .event(Event::AttentionChanged { revision });
        }
        tokio::select! {
            changed = status.changed() => if changed.is_err() { return },
            changed = doctor.changed() => if changed.is_err() { return },
            changed = replaced.changed() => if changed.is_err() { return },
            closed = durable(&mut live) => if closed { return },
            () = tokio::time::sleep(timing.every) => {}
            _ = stopping.wait_for(|stopping| *stopping) => return,
        }
    }
}

/// Waits for the next durable event, such as a job that finished; `true` once the store is gone. Samples, log lines
/// and the worker's own `attention.changed` do not wake it.
async fn durable(live: &mut broadcast::Receiver<Arc<EventEnvelope>>) -> bool {
    loop {
        match live.recv().await {
            Ok(envelope) if envelope.event.is_durable() => return false,
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => return false,
            Err(broadcast::error::RecvError::Closed) => return true,
        }
    }
}

/// The insights of the default window as `GET /v1/insights` answers them now; `None`, logged, when they cannot be read.
async fn read_insights(state: &AppState) -> Option<Insights> {
    let now = super::history::now() * 1000;
    match super::history::insight_runs(state, INSIGHT_DAYS, now).await {
        Ok((runs, config)) => Some(insights::insights(&runs, &config, INSIGHT_DAYS, now)),
        Err(message) => {
            warn!("attention: cannot read the insights, so none is listed: {message}");
            None
        }
    }
}
