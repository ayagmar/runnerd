//! The runs whose runner failed one of their jobs, which runnerd may re-run (ADR 0153), and the repositories whose
//! re-runs GitHub refused. One entry per run: the newest attempt a fault was seen in, and whether a check of it is due
//! or it is settled. Several faults of one attempt meet in its entry, so they give one re-run.
//!
//! It decides nothing about GitHub: whether to send is the stateless rule in `rerun`, which reads the run's attempt
//! right before the send. It survives no restart, and need not: a restart reads the history's faults back, and the
//! run's attempt says which were re-run. A reload keeps it; a refusal holds only for the config revision it came
//! under, and for [`HELD`] at most, so a token given the permission is tried again within the hour. It never sends
//! anything itself.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use crate::api::RerunRefused;
use crate::sync::lock;

/// How long a run is followed after its fault was seen: a run that has not completed by then is left to its owner.
pub(super) const FOLLOWED: Duration = Duration::from_hours(6);

/// How many checks of one run may fail, GitHub unreachable or answering a server error, before it is left.
pub(super) const TRIES_MAX: u32 = 5;

/// How long a refusal holds a repository's re-runs, unless the config changes first: one refused request an hour at
/// most.
pub(super) const HELD: Duration = Duration::from_hours(1);

/// A workflow run of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct RunKey {
    pub(super) repository: String,
    pub(super) run: u64,
}

/// A run whose check is due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Due {
    /// Which run.
    pub(super) key: RunKey,
    /// The attempt its runner failed a job in.
    pub(super) attempt: u32,
    /// The first job of that attempt its runner failed that runnerd saw.
    pub(super) job: u64,
}

/// Where one run is.
#[derive(Debug, Clone, Copy)]
enum Stage {
    /// A check is due at `at`; `failed` checks failed before it. A firm time (a backoff, a hold, a rate limit) is
    /// never brought forward by another fault.
    Due {
        at: Instant,
        failed: u32,
        firm: bool,
    },
    /// Nothing more to do for this attempt: it was re-run, by runnerd or a person, or must not be.
    Settled,
}

#[derive(Debug, Clone, Copy)]
struct Fault {
    attempt: u32,
    job: u64,
    seen: Instant,
    stage: Stage,
}

/// Why a repository's re-runs are refused, and under which config revision.
#[derive(Debug, Clone)]
struct Refusal {
    revision: u64,
    until: Instant,
    refused: RerunRefused,
}

/// A repository's re-runs held by a refusal.
#[derive(Debug, Clone)]
pub(super) struct Held {
    /// When the hold ends.
    pub(super) until: Instant,
    /// What GitHub refused.
    pub(super) refused: RerunRefused,
}

/// The runs and refusals. Only its methods write it, each in one critical section.
#[derive(Default)]
struct Book {
    runs: HashMap<RunKey, Fault>,
    refusals: BTreeMap<String, Refusal>,
    /// Until when GitHub's rate limit stops every read and send, once one answered for it.
    cooldown: Option<Instant>,
}

/// The runs runnerd follows for a re-run, and the repositories GitHub refused one for.
#[derive(Default)]
pub(super) struct Faults {
    // Locked with `sync::lock`: every critical section is a few lookups, inserts or removes.
    book: Mutex<Book>,
}

impl Faults {
    /// Notes that the runner failed job `job` of attempt `attempt` of `key`, at `now`. A first fault of an attempt
    /// makes its check due now. Another of the same attempt brings a check that waits forward to now, as the history
    /// may have closed it after GitHub said the run completed; one of an earlier attempt changes nothing.
    pub(super) fn seen(&self, key: RunKey, attempt: u32, job: u64, now: Instant) {
        lock(&self.book).seen(key, attempt, job, now);
    }

    /// The runs whose check is due at `now`, and leaves those followed longer than [`FOLLOWED`].
    pub(super) fn due(&self, now: Instant) -> Vec<Due> {
        lock(&self.book).due(now)
    }

    /// When the next check is due, if one is.
    pub(super) fn next(&self) -> Option<Instant> {
        lock(&self.book).next()
    }

    /// Settles `due`'s attempt: nothing more is done for it.
    pub(super) fn settle(&self, due: &Due) {
        lock(&self.book).settle(due);
    }

    /// Checks `due` again at `at`, as when its run has not completed or the rate limit is used up: no check failed.
    pub(super) fn wait(&self, due: &Due, at: Instant) {
        lock(&self.book).wait(due, at);
    }

    /// Checks `due` again at `at` because this check failed; after [`TRIES_MAX`] failed checks, settles it instead.
    /// Whether it will be checked again.
    pub(super) fn failed(&self, due: &Due, at: Instant) -> bool {
        lock(&self.book).failed(due, at)
    }

    /// Checks `due` again at `at` and no sooner, as for a hold or a rate limit: no check failed.
    pub(super) fn hold(&self, due: &Due, at: Instant) {
        lock(&self.book).hold(due, at);
    }

    /// Stops every read and send until `until`: GitHub's rate limit answered.
    pub(super) fn cool(&self, until: Instant) {
        lock(&self.book).cool(until);
    }

    /// Until when reads and sends are stopped, at `now`, if they are.
    pub(super) fn cooling(&self, now: Instant) -> Option<Instant> {
        lock(&self.book).cooling(now)
    }

    /// Notes that GitHub refused `repository`'s re-runs under config revision `revision` at `now`, as `refused` says:
    /// they are held for [`HELD`]. When the hold ends.
    pub(super) fn refuse(
        &self,
        repository: &str,
        revision: u64,
        refused: RerunRefused,
        now: Instant,
    ) -> Instant {
        lock(&self.book).refuse(repository, revision, refused, now)
    }

    /// Why `repository`'s re-runs are held under config revision `revision` at `now`, and until when; `None` once the
    /// config changed or the hold ended.
    pub(super) fn held(&self, repository: &str, revision: u64, now: Instant) -> Option<Held> {
        lock(&self.book).held(repository, revision, now)
    }
}

impl Book {
    /// Notes that the runner failed job `job` of attempt `attempt` of `key`, at `now`. A first fault of an attempt
    /// makes its check due now. Another of the same attempt brings a check that waits forward to now, as the history
    /// may have closed it after GitHub said the run completed; one of an earlier attempt changes nothing.
    fn seen(&mut self, key: RunKey, attempt: u32, job: u64, now: Instant) {
        if let Some(fault) = self.runs.get_mut(&key)
            && fault.attempt >= attempt
        {
            if fault.attempt == attempt
                && let Stage::Due {
                    at,
                    failed,
                    firm: false,
                } = fault.stage
            {
                fault.stage = Stage::Due {
                    at: at.min(now),
                    failed,
                    firm: false,
                };
            }
            return;
        }
        self.runs.insert(
            key,
            Fault {
                attempt,
                job,
                seen: now,
                stage: Stage::Due {
                    at: now,
                    failed: 0,
                    firm: false,
                },
            },
        );
    }

    /// The runs whose check is due at `now`, and leaves those followed longer than [`FOLLOWED`].
    fn due(&mut self, now: Instant) -> Vec<Due> {
        self.runs
            .retain(|_, fault| now.saturating_duration_since(fault.seen) < FOLLOWED);
        let mut due: Vec<Due> = self
            .runs
            .iter()
            .filter(|(_, fault)| matches!(fault.stage, Stage::Due { at, .. } if at <= now))
            .map(|(key, fault)| Due {
                key: key.clone(),
                attempt: fault.attempt,
                job: fault.job,
            })
            .collect();
        due.sort_by(|a, b| a.key.cmp(&b.key));
        due
    }

    /// When the next check is due, if one is.
    fn next(&self) -> Option<Instant> {
        self.runs
            .values()
            .filter_map(|fault| match fault.stage {
                Stage::Due { at, .. } => Some(at),
                Stage::Settled => None,
            })
            .min()
    }

    /// Settles `due`'s attempt: nothing more is done for it.
    fn settle(&mut self, due: &Due) {
        self.with(due, |fault| fault.stage = Stage::Settled);
    }

    /// Checks `due` again at `at`, as when its run has not completed or the rate limit is used up: no check failed.
    fn wait(&mut self, due: &Due, at: Instant) {
        self.with(due, |fault| {
            if let Stage::Due { failed, .. } = fault.stage {
                fault.stage = Stage::Due {
                    at,
                    failed,
                    firm: false,
                };
            }
        });
    }

    /// Checks `due` again at `at` and no sooner, as for a hold or a rate limit: no check failed.
    fn hold(&mut self, due: &Due, at: Instant) {
        self.with(due, |fault| {
            if let Stage::Due { failed, .. } = fault.stage {
                fault.stage = Stage::Due {
                    at,
                    failed,
                    firm: true,
                };
            }
        });
    }

    /// Stops every read and send until `until`.
    fn cool(&mut self, until: Instant) {
        self.cooldown = Some(self.cooldown.map_or(until, |was| was.max(until)));
    }

    /// Until when reads and sends are stopped, at `now`, if they are.
    fn cooling(&mut self, now: Instant) -> Option<Instant> {
        match self.cooldown {
            Some(until) if now < until => Some(until),
            _ => {
                self.cooldown = None;
                None
            }
        }
    }

    /// Checks `due` again at `at` because this check failed; after [`TRIES_MAX`] failed checks, settles it instead.
    /// Whether it will be checked again.
    fn failed(&mut self, due: &Due, at: Instant) -> bool {
        let mut again = false;
        self.with(due, |fault| {
            if let Stage::Due { failed, .. } = fault.stage {
                let failed = failed + 1;
                again = failed < TRIES_MAX;
                fault.stage = if again {
                    Stage::Due {
                        at,
                        failed,
                        firm: true,
                    }
                } else {
                    Stage::Settled
                };
            }
        });
        again
    }

    /// Notes that GitHub refused `repository`'s re-runs under config revision `revision` at `now`, as `refused` says:
    /// they are held for [`HELD`]. When the hold ends.
    fn refuse(
        &mut self,
        repository: &str,
        revision: u64,
        refused: RerunRefused,
        now: Instant,
    ) -> Instant {
        let until = now + HELD;
        self.refusals.insert(
            repository.to_owned(),
            Refusal {
                revision,
                until,
                refused,
            },
        );
        until
    }

    /// Why `repository`'s re-runs are held under config revision `revision` at `now`, and until when; `None` once the
    /// config changed or the hold ended.
    fn held(&mut self, repository: &str, revision: u64, now: Instant) -> Option<Held> {
        let refusal = self.refusals.get(repository)?;
        if refusal.revision == revision && now < refusal.until {
            return Some(Held {
                until: refusal.until,
                refused: refusal.refused.clone(),
            });
        }
        self.refusals.remove(repository);
        None
    }

    /// Changes `due`'s entry with `change`, if it is still of `due`'s attempt.
    fn with(&mut self, due: &Due, change: impl FnOnce(&mut Fault)) {
        if let Some(fault) = self
            .runs
            .get_mut(&due.key)
            .filter(|fault| fault.attempt == due.attempt)
        {
            change(fault);
        }
    }
}
