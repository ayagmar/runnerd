//! The jobs the engine follows on GitHub that no runner of this machine runs now: queued ones a listing recorded,
//! and closing ones whose runner went before GitHub said they completed. It owns that value, one stage per job, and
//! must not call GitHub, the store or log: `history` reads the jobs and records what GitHub says. Which runner holds a
//! job is not here: that is each slot's occupant.

use std::collections::BTreeMap;
#[cfg(test)]
use std::time::Duration;

use tokio::time::Instant;

use super::history::LOST_AFTER;
use crate::time;

/// The jobs followed, each in one stage (ADR 0118).
///
/// Nothing of it survives a restart: `recover` follows again the jobs the history holds queued, and closes those
/// whose runner's container is gone, their wait counted from the restart. A reload keeps every job, including those
/// of a repository it removed: GitHub still says what became of them.
#[derive(Debug, Default)]
pub(super) struct Ledger(BTreeMap<u64, Stage>);

#[derive(Debug)]
enum Stage {
    /// Recorded queued, and no runner of this machine is known to have taken it.
    Queued {
        repository: String,
        /// Since when GitHub failed every read of it once it left the queue; `None` while it answers.
        unanswered_since: Option<Instant>,
    },
    /// Its runner went, and GitHub has not said it completed.
    Closing {
        repository: String,
        /// When its runner was found gone, on Tokio's clock, which tests can advance.
        since: Instant,
        /// The same, in milliseconds since the epoch: when the job ended, if GitHub never says.
        since_ms: u64,
    },
}

/// How a read of a queued job failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadFailure {
    /// GitHub's rate limit is used up: no read was made.
    RateLimited,
    /// GitHub says it is gone, as when its run was deleted.
    Gone,
    /// Any other error.
    Failed,
}

/// What a failed read of a queued job comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unanswered {
    /// It is no longer followed as queued: a runner took it, or its runner went, meanwhile.
    NotQueued,
    /// It stays followed.
    Waits,
    /// It is no longer followed: GitHub says it is gone, or failed every read of it for `LOST_AFTER`.
    GivesUp,
}

/// Where a closing job's wait for GitHub is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wait {
    /// Shorter than `LOST_AFTER`.
    Waiting,
    /// `LOST_AFTER` or longer, since its runner went at this many milliseconds since the epoch.
    Over { since_ms: u64 },
}

impl Ledger {
    /// Follows `id` of `repository` as queued, unless it is followed already: a queued job keeps its clock, and a
    /// closing one stays closing, though GitHub may list it queued a while after its runner went.
    pub(super) fn follow_queued(&mut self, id: u64, repository: &str) {
        self.0.entry(id).or_insert_with(|| Stage::Queued {
            repository: repository.to_owned(),
            unanswered_since: None,
        });
    }

    /// Stops following `id` as queued, as a runner took it or another machine's ran it. A closing job stays.
    pub(super) fn stop_queued(&mut self, id: u64) {
        if matches!(self.0.get(&id), Some(Stage::Queued { .. })) {
            self.0.remove(&id);
        }
    }

    /// The runner of `id` went just now: it waits for GitHub to say it completed, from now on, whatever it was.
    pub(super) fn close(&mut self, id: u64, repository: &str) {
        self.0.insert(
            id,
            Stage::Closing {
                repository: repository.to_owned(),
                since: Instant::now(),
                since_ms: time::now_ms(),
            },
        );
    }

    /// GitHub answered a read of queued `id`, or it was not read: the clock of failed reads starts over (ADR 0109).
    pub(super) fn answered(&mut self, id: u64) {
        if let Some(Stage::Queued {
            unanswered_since, ..
        }) = self.0.get_mut(&id)
        {
            *unanswered_since = None;
        }
    }

    /// A read of `id` as a queued job failed: whether it stays followed. A used-up rate limit is no read, and starts
    /// the clock over.
    pub(super) fn read_failed(&mut self, id: u64, failure: ReadFailure) -> Unanswered {
        let Some(Stage::Queued {
            unanswered_since, ..
        }) = self.0.get_mut(&id)
        else {
            return Unanswered::NotQueued;
        };
        let gives_up = match failure {
            ReadFailure::RateLimited => {
                *unanswered_since = None;
                false
            }
            ReadFailure::Gone => true,
            ReadFailure::Failed => {
                unanswered_since.get_or_insert_with(Instant::now).elapsed() >= LOST_AFTER
            }
        };
        if gives_up {
            self.0.remove(&id);
            Unanswered::GivesUp
        } else {
            Unanswered::Waits
        }
    }

    /// The queued jobs `read` picks, with their repositories; the clock of each one it leaves is started over, as it
    /// is not read this tick.
    pub(super) fn queued_to_read(
        &mut self,
        read: impl Fn(u64, &str) -> bool,
    ) -> Vec<(String, u64)> {
        let mut picked = Vec::new();
        for (id, stage) in &mut self.0 {
            if let Stage::Queued {
                repository,
                unanswered_since,
            } = stage
            {
                if read(*id, repository) {
                    picked.push((repository.clone(), *id));
                } else {
                    *unanswered_since = None;
                }
            }
        }
        picked
    }

    /// The closing jobs, with their repositories.
    pub(super) fn closing(&self) -> Vec<(String, u64)> {
        self.0
            .iter()
            .filter_map(|(id, stage)| match stage {
                Stage::Closing { repository, .. } => Some((repository.clone(), *id)),
                Stage::Queued { .. } => None,
            })
            .collect()
    }

    /// Where the wait of `id` is, if it is closing.
    pub(super) fn wait(&self, id: u64) -> Option<Wait> {
        match self.0.get(&id)? {
            Stage::Closing {
                since, since_ms, ..
            } if since.elapsed() >= LOST_AFTER => Some(Wait::Over {
                since_ms: *since_ms,
            }),
            Stage::Closing { .. } => Some(Wait::Waiting),
            Stage::Queued { .. } => None,
        }
    }

    /// Stops following closing `id`: GitHub said it completed, or its wait is over. A queued job stays.
    pub(super) fn closed(&mut self, id: u64) {
        if matches!(self.0.get(&id), Some(Stage::Closing { .. })) {
            self.0.remove(&id);
        }
    }
}

#[cfg(test)]
impl Ledger {
    pub(super) fn is_queued(&self, id: u64) -> bool {
        matches!(self.0.get(&id), Some(Stage::Queued { .. }))
    }

    pub(super) fn is_closing(&self, id: u64) -> bool {
        matches!(self.0.get(&id), Some(Stage::Closing { .. }))
    }

    /// As if the runner of closing `id` was found gone at `ms` since the epoch, apart from the clock that ends the wait.
    pub(super) fn found_gone_at(&mut self, id: u64, ms: u64) {
        if let Some(Stage::Closing { since_ms, .. }) = self.0.get_mut(&id) {
            *since_ms = ms;
        }
    }

    /// As if `by` passed on every job's clock: since a closing job's runner went, and since GitHub began failing the
    /// reads of a queued one.
    pub(super) fn age(&mut self, by: Duration) {
        for stage in self.0.values_mut() {
            match stage {
                Stage::Queued {
                    unanswered_since, ..
                } => {
                    if let Some(since) = unanswered_since {
                        *since -= by;
                    }
                }
                Stage::Closing {
                    since, since_ms, ..
                } => {
                    *since -= by;
                    *since_ms =
                        since_ms.saturating_sub(u64::try_from(by.as_millis()).unwrap_or(u64::MAX));
                }
            }
        }
    }
}
