//! Hints that a runner's container stopped, from Docker's events, so the engine frees its slot and starts the next
//! runner within seconds rather than at its next poll (ADR 0095). A task follows the events and notes each runner
//! container that stops; on every connection it notes that any may have stopped meanwhile. It must not free a slot or
//! decide anything: a hint only wakes the engine, whose inspect of the container decides, and the tick's inspect of
//! every runner stays the reconciliation for hints Docker never sent.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::docker::DockerApi;
use crate::sync::lock;

/// Containers noted at most between two passes; past that a pass inspects every runner, as after a reconnect.
const MAX_NOTED: usize = 64;

/// The first wait before following Docker's events again after the stream failed or ended; it doubles up to
/// `MAX_BACKOFF` while it keeps failing.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);

/// The longest wait between two tries to follow Docker's events.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long a connection to Docker's events lasts before the runners that stopped while none was followed are
/// inspected: long enough for the request to have reached Docker, which says nothing until an event comes.
const SETTLE: Duration = Duration::from_secs(1);

/// How long a stream of events must have lasted for its end to count as a new failure streak's first.
const STEADY: Duration = Duration::from_mins(1);

/// The runner containers noted stopped since the engine last took them, and the wake-up of the engine.
#[derive(Debug, Default)]
pub(super) struct Exits {
    noted: Mutex<Noted>,
    wake: Notify,
}

/// What `Exits` noted since the engine last took it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Noted {
    /// The IDs of the containers Docker said stopped.
    pub(super) containers: BTreeSet<String>,
    /// Any runner may have stopped unseen: the events were not followed for a while, or too many came.
    pub(super) any: bool,
}

impl Noted {
    /// Whether nothing was noted.
    pub(super) fn is_empty(&self) -> bool {
        !self.any && self.containers.is_empty()
    }
}

impl Exits {
    /// Notes that `container` stopped and wakes the engine.
    pub(super) fn stopped(&self, container: String) {
        {
            let mut noted = lock(&self.noted);
            if noted.containers.len() < MAX_NOTED {
                noted.containers.insert(container);
            } else {
                noted.any = true;
            }
        }
        self.wake.notify_one();
    }

    /// Notes that any runner may have stopped unseen and wakes the engine.
    pub(super) fn any(&self) {
        lock(&self.noted).any = true;
        self.wake.notify_one();
    }

    /// Whether `container` was noted stopped and not taken yet.
    pub(super) fn noted_stopped(&self, container: &str) -> bool {
        lock(&self.noted).containers.contains(container)
    }

    /// Forgets that any runner may have stopped unseen: every runner is about to be inspected.
    pub(super) fn inspecting_all(&self) {
        lock(&self.noted).any = false;
    }

    /// Takes what was noted since the last call.
    pub(super) fn take(&self) -> Noted {
        std::mem::take(&mut *lock(&self.noted))
    }

    /// Waits until something is noted that the engine did not take yet. A note made between the engine's last
    /// `take` and this call wakes it at once.
    pub(super) async fn noted(&self) {
        self.wake.notified().await;
    }
}

/// The task following Docker's events, which stops with the engine.
#[derive(Debug)]
pub(super) struct Watcher(JoinHandle<()>);

impl Watcher {
    /// Follows Docker's events for runner containers that stop, noting them in `exits`, until dropped.
    pub(super) fn start<D: DockerApi + 'static>(docker: Arc<D>, exits: Arc<Exits>) -> Watcher {
        Watcher(tokio::spawn(follow(docker, exits)))
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Follows Docker's events, again after each failure or end, waiting `FIRST_BACKOFF` and doubling up to
/// `MAX_BACKOFF` while it keeps failing. Once a connection lasted `SETTLE`, it notes that any runner may have stopped
/// while none was followed, so the engine inspects them all: Docker keeps no history of events to replay.
async fn follow<D: DockerApi>(docker: Arc<D>, exits: Arc<Exits>) {
    let mut backoff = FIRST_BACKOFF;
    let mut failing = false;
    loop {
        let followed = tokio::time::Instant::now();
        let mut events = docker.exits();
        // The request goes out at the first poll, and Docker answers only with an event or an error.
        let settle = tokio::time::sleep(SETTLE);
        tokio::pin!(settle);
        let mut settled = false;
        let ended = loop {
            tokio::select! {
                () = &mut settle, if !settled => {
                    settled = true;
                    exits.any();
                }
                event = events.next() => match event {
                    Some(Ok(container)) => exits.stopped(container),
                    Some(Err(error)) => break error.to_string(),
                    None => break "the stream ended".to_owned(),
                },
            }
        };
        if followed.elapsed() >= STEADY {
            backoff = FIRST_BACKOFF;
            failing = false;
        }
        // Once per streak: a Docker that stays down would say so every `MAX_BACKOFF`.
        if failing {
            debug!("cannot follow Docker's events still: {ended}");
        } else {
            warn!(
                "cannot follow Docker's events ({ended}); runners that end are found at the next poll until they \
                 are followed again"
            );
            failing = true;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
