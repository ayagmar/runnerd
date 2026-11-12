//! `GET /v1/attention`: what needs attention now, kept by one worker that owns the snapshot (ADR 0132). The worker
//! words nothing itself: `api::attention_items` decides every item from the engine's status, the doctor's last run
//! (`doctor_cache`) and the insights. It looks again whenever the status, the doctor's run or the config changes, on
//! each durable event, and every `Timing::every`, so a condition that comes or goes with time alone shows. It reads
//! the insights at most once a `Timing::insights`, and again at once when the config they were read with is no longer
//! the one running, or the jobs waiting for the host's memory are not those they were read with. The revision moves
//! only when the content does, also when one item replaces another, and each move is sent as `attention.changed`.
//!
//! It must not change the engine, the store or the config: it reads them and publishes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// The snapshot and its revision. Only `update` writes it. Nothing of it survives a restart: the next process has an
/// instance of its own and counts its revisions from 1 again, so a client never mistakes a new snapshot for the one it
/// holds, whatever the clock did between.
struct Snapshot {
    shown: Attention,
}

impl Snapshot {
    /// Takes `items` in place of what it shows: the new revision when they differ, `None` when they say the same.
    fn update(&mut self, items: Vec<api::AttentionItem>) -> Option<u64> {
        let next = Attention::new(self.shown.instance.clone(), self.shown.revision + 1, items);
        if next.same_content(&self.shown) {
            return None;
        }
        self.shown = next;
        Some(self.shown.revision)
    }
}

/// The insights the worker read last, and what they were read with. Replaced whole by each read.
struct Read {
    at: Instant,
    /// The revision of the config they were read with.
    config: String,
    /// The jobs waiting for the host's memory when they were read, which the host-held insight counts.
    held: Vec<u64>,
    /// `None` when the read failed: no insight is listed then.
    insights: Option<Insights>,
}

impl Read {
    /// Whether they still stand for `config` and `status`: read within `Timing::insights`, with the config running,
    /// while the same jobs wait for the host's memory.
    fn stands(&self, config: &str, status: &api::Status, timing: Timing) -> bool {
        self.at.elapsed() < timing.insights && self.config == config && self.held == held(status)
    }
}

/// The jobs waiting for the host's memory in `status`, in order.
fn held(status: &api::Status) -> Vec<u64> {
    let mut held: Vec<u64> = status
        .queued
        .iter()
        .filter(|job| matches!(job.waiting, Some(api::Waiting::HostMemory { .. })))
        .map(|job| job.id)
        .collect();
    held.sort_unstable();
    held
}

/// A new server's instance: the process's ID, its start in nanoseconds and a count of the servers it started, so no
/// two are the same, whatever the clock does.
fn instance() -> String {
    static STARTED: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!(
        "{:x}-{nanos:x}-{}",
        std::process::id(),
        STARTED.fetch_add(1, Ordering::Relaxed)
    )
}

/// What needs attention before the worker's first look, at revision 1 of a new instance: the status as it is and no
/// doctor run, so never an all clear.
pub(super) fn first(status: &api::Status) -> Attention {
    let doctor = Doctor {
        checks: Vec::new(),
        state: Some(DoctorState::NotRun),
    };
    Attention::new(
        instance(),
        1,
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
        let revision = replaced.borrow_and_update().clone();
        let current = status.borrow_and_update().clone();
        if !read
            .as_ref()
            .is_some_and(|read| read.stands(&revision, &current, timing))
        {
            read = Some(Read {
                at: Instant::now(),
                config: revision.clone(),
                held: held(&current),
                insights: read_insights(&state).await,
            });
        }
        let now = super::history::now();
        let report = doctor
            .borrow_and_update()
            .report(&revision, now, state.doctors.timing());
        let items = api::attention_items(
            &current,
            &report,
            read.as_ref().and_then(|read| read.insights.as_ref()),
            now,
        );
        if let Some(revision) = snapshot.update(items) {
            published.send_replace(snapshot.shown.clone());
            state.handle.store.event(Event::AttentionChanged {
                instance: snapshot.shown.instance.clone(),
                revision,
            });
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

#[cfg(test)]
mod tests;
