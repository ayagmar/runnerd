//! The doctor's checks for the config the daemon runs with, kept by one worker (ADR 0132). `GET /v1/doctor` and the
//! attention worker read the last run here instead of each reading the host. The worker runs one check of the host
//! at a time: at the start, after each config replaced (reloads that come while a run goes on make one run after it,
//! and its result, for a config no longer current, is dropped), when a request finds no recent run, and every
//! `Timing::refresh`. A run reads the job history from the store, then everything it asks the host, the slice's
//! `MemoryMax` included, on one blocking thread: each command's process group is killed past
//! `doctor::READ_DEADLINE`, each Docker request dropped. A run not done `Timing::deadline` after it began is reported
//! failed, and the next starts only once its blocking work ended. A request waits for a run at most `Timing::answer`,
//! under every client's timeout, and otherwise answers the last run as it stands. Nothing of it survives a restart; at
//! a stop the worker ends, and the daemon kills the commands still running (`doctor::stop_commands`).
//!
//! It must not change the host, the config or the engine: it reads.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use runnerd_core::api::{self, Check};
use runnerd_core::config::{Config, revision};
use runnerd_core::sizing::{History, Window, advise};
use tokio::sync::{Notify, watch};
use tracing::warn;

use super::AppState;

/// Reads the host and runs the checks for `config` read from `path`, with what the sizing check reads of the job
/// history: `read_host`, or a test's. Called on a blocking thread.
pub(crate) type ReadHost =
    Arc<dyn Fn(Option<&Config>, &Path, Result<Recorded, String>) -> Vec<Check> + Send + Sync>;

/// What the sizing check reads of the job history, as `GET /v1/sizing` reads it: the slice's `MemoryMax` is the
/// host's, read with the rest of it.
pub(crate) struct Recorded {
    pub history: History,
    pub window: Window,
}

/// The doctor of `runnerd doctor`, on this host.
pub(crate) fn read_host(
    config: Option<&Config>,
    path: &Path,
    recorded: Result<Recorded, String>,
) -> Vec<Check> {
    let mut host = crate::doctor::Host::read(config, Some(path));
    let memory_max = host
        .slice
        .as_ref()
        .and_then(|slice| slice.as_ref().ok())
        .and_then(|slice| slice.memory_max)
        .map(|max| max.0);
    host.sizing = config.map(|config| {
        recorded.map(|recorded| advise(config, memory_max, &recorded.history, recorded.window))
    });
    crate::doctor::checks(&host, config)
}

/// The history the sizing check reads, from the store.
async fn recorded(state: &AppState) -> Result<Recorded, String> {
    let window = Window::new(None, super::history::now());
    let history = super::history::sizing_history(state, window).await?;
    Ok(Recorded { history, window })
}

/// How the worker paces its runs, and how old a run may be.
#[derive(Clone, Copy)]
pub(crate) struct Timing {
    /// A run this recent, of the config running, answers a request without a new one.
    pub fresh: Duration,
    /// How often it runs with nothing asking.
    pub refresh: Duration,
    /// A run older than this is stale.
    pub stale: Duration,
    /// How long one run may take, from reading the history to the last check, before it is reported failed.
    pub deadline: Duration,
    /// How long a request waits for a run before it answers the last as it stands: under every client's timeout.
    pub answer: Duration,
}

/// What the daemon runs with: a run every 10 minutes, stale after 15, one under a minute old answering a request,
/// which waits 20 s at most for a new one (`client::TIMEOUT` is 30 s).
pub(crate) const TIMING: Timing = Timing {
    fresh: Duration::from_mins(1),
    refresh: Duration::from_mins(10),
    stale: Duration::from_mins(15),
    deadline: Duration::from_mins(2),
    answer: Duration::from_secs(20),
};

/// How the doctor runs: what reads the host, and its pace.
#[derive(Clone)]
pub(crate) struct Setup {
    pub read: ReadHost,
    pub timing: Timing,
}

/// The daemon's own: this host, at [`TIMING`].
pub(crate) fn setup() -> Setup {
    Setup {
        read: Arc::new(read_host),
        timing: TIMING,
    }
}

/// The last run of the checks, as the worker publishes it. Only its methods here write it.
#[derive(Debug, Clone, Default)]
pub(super) struct Last(Run);

#[derive(Debug, Clone, Default)]
enum Run {
    /// No run has finished since the start.
    #[default]
    NotRun,
    /// A run for the config of `revision` finished at `at` (seconds since the Unix epoch).
    Done {
        revision: String,
        at: u64,
        outcome: Result<Vec<Check>, String>,
    },
}

impl Last {
    fn done(&mut self, revision: String, at: u64, outcome: Result<Vec<Check>, String>) {
        self.0 = Run::Done {
            revision,
            at,
            outcome,
        };
    }

    /// Whether a run for the config of `revision` finished at `since` or later, its checks or its failure.
    fn answers(&self, revision: &str, since: u64) -> bool {
        matches!(&self.0, Run::Done { revision: ran, at, .. } if ran == revision && *at >= since)
    }

    /// The checks as clients read them while the config of `revision` runs, at `now`: with the state that says
    /// whether they stand. A failed run keeps none, and a stale one keeps its checks, marked so.
    pub(super) fn report(&self, revision: &str, now: u64, timing: Timing) -> api::Doctor {
        match &self.0 {
            Run::NotRun => api::Doctor {
                checks: Vec::new(),
                state: Some(api::DoctorState::NotRun),
            },
            Run::Done {
                outcome: Err(error),
                at,
                ..
            } => api::Doctor {
                checks: Vec::new(),
                state: Some(api::DoctorState::Failed {
                    at: *at,
                    error: error.clone(),
                }),
            },
            Run::Done {
                revision: ran,
                at,
                outcome: Ok(checks),
            } => {
                let current = ran == revision && now.saturating_sub(*at) <= timing.stale.as_secs();
                api::Doctor {
                    checks: checks.clone(),
                    state: Some(if current {
                        api::DoctorState::Ready { at: *at }
                    } else {
                        api::DoctorState::Stale { at: *at }
                    }),
                }
            }
        }
    }
}

/// The handle every reader of the checks holds.
#[derive(Clone)]
pub(super) struct Doctors {
    last: watch::Receiver<Last>,
    /// Asks the worker for a run.
    wanted: Arc<Notify>,
    timing: Timing,
}

impl Doctors {
    /// The last run, which changes as runs finish.
    pub(super) fn subscribe(&self) -> watch::Receiver<Last> {
        self.last.clone()
    }

    pub(super) fn timing(&self) -> Timing {
        self.timing
    }

    /// The checks for the config of `revision()` at most `Timing::fresh` old: the last run's when it is, else those of
    /// a run asked for now, once it ends. Requests at once share that run. A run not done within `Timing::answer`
    /// goes on, and the last run's checks answer, in their state: not run, stale or failed.
    pub(super) async fn current(&self, revision: impl Fn() -> String) -> api::Doctor {
        let asked = super::history::now();
        let mut last = self.last.clone();
        let recent = asked.saturating_sub(self.timing.fresh.as_secs());
        if !last.borrow().answers(&revision(), recent) {
            self.wanted.notify_one();
            let done = last.wait_for(|last| last.answers(&revision(), asked));
            let _ = tokio::time::timeout(self.timing.answer, done).await;
        }
        last.borrow()
            .report(&revision(), super::history::now(), self.timing)
    }
}

/// What the worker keeps: where it publishes its runs, what asks it for one, and how it runs.
pub(super) struct Worker {
    published: watch::Sender<Last>,
    wanted: Arc<Notify>,
    setup: Setup,
}

/// The readers' handle and the worker that serves it.
pub(super) struct Cache {
    pub doctors: Doctors,
    /// What `run` runs.
    pub worker: Worker,
}

/// A cache with no run yet.
pub(super) fn cache(setup: Setup) -> Cache {
    let (published, last) = watch::channel(Last::default());
    let wanted = Arc::new(Notify::new());
    Cache {
        doctors: Doctors {
            last,
            wanted: wanted.clone(),
            timing: setup.timing,
        },
        worker: Worker {
            published,
            wanted,
            setup,
        },
    }
}

/// Runs the checks until the server stops: at once, then on each config replaced, each request and each refresh.
pub(super) async fn run(state: AppState, worker: Worker) {
    let Worker {
        published,
        wanted,
        setup,
    } = worker;
    let mut replaced = state.config_revision.subscribe();
    let mut stopping = state.stopping.clone();
    // A run that went past its deadline, still reading the host: the next waits for it.
    let mut overdue: Option<tokio::task::JoinHandle<Vec<Check>>> = None;
    loop {
        if let Some(run) = overdue.take() {
            tokio::select! {
                _ = run => {}
                _ = stopping.wait_for(|stopping| *stopping) => return,
            }
        }
        let (text, path) = {
            let file = state.config.lock().await;
            (file.text.clone(), file.path.clone())
        };
        let ran_for = revision(&text);
        replaced.borrow_and_update();
        let deadline = setup.timing.deadline;
        // One deadline for the whole run, the history's read included.
        let until = tokio::time::Instant::now() + deadline;
        let recorded = tokio::time::timeout_at(until, recorded(&state))
            .await
            .unwrap_or_else(|_| Err("the job history did not answer in time".to_owned()));
        let read = setup.read.clone();
        let mut task = tokio::task::spawn_blocking(move || {
            read(Config::parse(&text).ok().as_ref(), &path, recorded)
        });
        let outcome = tokio::select! {
            done = tokio::time::timeout_at(until, &mut task) => match done {
                Ok(Ok(checks)) => Ok(checks),
                Ok(Err(join_error)) => Err(format!("it stopped: {join_error}")),
                Err(_) => {
                    overdue = Some(task);
                    Err(format!(
                        "it took longer than {}",
                        api::duration(deadline.as_secs())
                    ))
                }
            },
            _ = stopping.wait_for(|stopping| *stopping) => return,
        };
        if let Err(error) = &outcome {
            warn!("doctor: the host's checks did not finish: {error}");
        }
        // A config replaced while it ran: its checks are of the one before, and the next run starts at once.
        if *replaced.borrow() != ran_for {
            continue;
        }
        let at = super::history::now();
        published.send_modify(|last| last.done(ran_for.clone(), at, outcome));
        let refresh = tokio::time::sleep(setup.timing.refresh);
        tokio::pin!(refresh);
        loop {
            tokio::select! {
                changed = replaced.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                () = wanted.notified() => {
                    // A request made while the last run went on is answered by it: it asks for no second run.
                    let recent = super::history::now().saturating_sub(setup.timing.fresh.as_secs());
                    if !published.borrow().answers(&ran_for, recent) {
                        break;
                    }
                }
                () = &mut refresh => break,
                _ = stopping.wait_for(|stopping| *stopping) => return,
            }
        }
    }
}
