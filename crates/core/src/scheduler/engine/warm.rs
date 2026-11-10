//! Warm runners (ADR 0050, 0107): reads what `scheduler::warm` learns from the job history every few minutes and
//! after a reload, notes each job it sees queued, taken or finished, follows the workflow runs those jobs belong to,
//! and says how many warm runners each repository wants. It must not start or stop runners: `plan` decides with what
//! it says.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use tracing::warn;

use super::slot::Holder;
use super::{Engine, Purpose};
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::scheduler::warm::{self, Attempt, Learned, Runs};
use crate::scheduler::{Pool, matches};
use crate::time;

/// How often the history is read again.
const REREAD: Duration = Duration::from_secs(300);

/// What warm runners keep between ticks; only its methods change what the history taught. A restart keeps none of it,
/// and the first tick reads the history. A reload marks the read stale, and the next tick reads it again.
#[derive(Debug, Default)]
pub(super) struct Warm {
    /// `None` before the first read, when no repository wants any.
    read: Option<HistoryRead>,
    /// The workflow runs seen lately.
    runs: Runs,
}

/// What the history taught, and whether to read it again.
#[derive(Debug)]
struct HistoryRead {
    learned: Learned,
    freshness: Freshness,
}

/// When the history is due to be read again.
#[derive(Debug, Clone, Copy)]
enum Freshness {
    /// Read at this time: due once `REREAD` old.
    ReadAt(SystemTime),
    /// Due at the next tick.
    Stale,
}

impl Warm {
    /// Whether the last read still stands.
    fn is_fresh(&self) -> bool {
        self.read.as_ref().is_some_and(|read| match read.freshness {
            Freshness::ReadAt(at) => at.elapsed().is_ok_and(|age| age < REREAD),
            Freshness::Stale => false,
        })
    }

    /// What the history taught, once it was read.
    fn learned(&self) -> Option<&Learned> {
        self.read.as_ref().map(|read| &read.learned)
    }

    /// Takes a read of the history made now, keeping what was noted since the last.
    fn took_in(&mut self, mut learned: Learned) {
        if let Some(older) = self.learned() {
            learned.absorb(older);
        }
        self.read = Some(HistoryRead {
            learned,
            freshness: Freshness::ReadAt(SystemTime::now()),
        });
    }

    /// Has the next tick read the history again.
    fn mark_stale(&mut self) {
        if let Some(read) = &mut self.read {
            read.freshness = Freshness::Stale;
        }
    }

    /// Whether what was learned counts `repository` active at `now` (ms) (`Learned::active`); not before the first
    /// read.
    pub(super) fn active(&self, repository: &str, now: u64) -> bool {
        self.learned()
            .is_some_and(|learned| learned.active(repository, now))
    }

    /// Notes that `repository` was active at `at` (ms).
    fn note_active(&mut self, repository: &str, at: u64) {
        if let Some(read) = &mut self.read {
            read.learned.note(repository, at);
        }
    }
}

#[cfg(test)]
impl Warm {
    /// As if the history had just been read and taught exactly `learned`.
    pub(super) fn testing_learned(&mut self, learned: Learned) {
        self.read = Some(HistoryRead {
            learned,
            freshness: Freshness::ReadAt(SystemTime::now()),
        });
    }

    /// As if the last read of the history were `REREAD` old.
    pub(super) fn testing_age_read(&mut self) {
        if let Some(read) = &mut self.read {
            read.freshness = Freshness::ReadAt(SystemTime::now() - REREAD);
        }
    }
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Reads the history again once the last read is `REREAD` old. A read that fails keeps the last one.
    pub(super) async fn refresh_warm(&mut self) {
        if self.warm.is_fresh() {
            return;
        }
        let now = time::now_ms();
        match self.store.warm(now.saturating_sub(warm::LOOKBACK)).await {
            Ok(seen) => {
                let config = &self.config;
                let learned = Learned::new(&seen, now, |seen| {
                    config
                        .pool(&seen.repository, &seen.labels)
                        .map(|pool| pool.profile.clone())
                });
                self.warm.took_in(learned);
            }
            Err(error) => warn!("cannot read the job history for warm runners: {error}"),
        }
        self.warm.runs.expire(now);
    }

    /// Reads the history again at the next tick, as a reload may have moved labels to other pools. What was noted
    /// since the last read is kept (`Learned::absorb`).
    pub(super) fn reread_warm(&mut self) {
        self.warm.mark_stale();
    }

    /// Notes that a job of `repository` finished now: the repository stays active `warm::ACTIVE_AFTER` past it.
    pub(super) fn note_finished(&mut self, repository: &str) {
        self.warm.note_active(repository, time::now_ms());
    }

    /// Notes a job for `pool` queued or taken at `at` (ms), of `attempt`, with its workflow when known.
    pub(super) fn note_job(
        &mut self,
        pool: &Pool,
        attempt: Attempt,
        job: u64,
        workflow: Option<&str>,
        at: u64,
    ) {
        self.warm.note_active(&pool.repository, at);
        self.warm.runs.note(pool, attempt, job, workflow, at);
    }

    /// The latest attempt of run `run_id` seen, or its first: a runner's capture tells only the run.
    pub(super) fn latest_attempt(&self, run_id: u64) -> Attempt {
        self.warm.runs.latest(run_id)
    }

    /// Moves a job a runner took to the attempt its details say, if it was noted under another of its run's.
    pub(super) fn place_job(
        &mut self,
        pool: &Pool,
        attempt: Attempt,
        job: u64,
        workflow: Option<&str>,
    ) {
        self.warm
            .runs
            .place(pool, attempt, job, workflow, time::now_ms());
    }

    /// Notes the workflow of run `run_id`, if it is followed, as a running job's details say.
    pub(super) fn note_workflow(&mut self, run_id: u64, workflow: Option<&str>) {
        if let Some(workflow) = workflow {
            self.warm.runs.name(run_id, workflow);
        }
    }

    /// Notes the queued jobs of the last listing that the repositories' runners may run.
    pub(super) fn note_queued(&mut self) {
        let mut seen = Vec::new();
        for repository in &self.config.repositories {
            let Some(state) = self.repositories.listed(&repository.name) else {
                continue;
            };
            seen.extend(
                state
                    .listing()
                    .jobs()
                    .iter()
                    .filter(|job| matches(job, &repository.labels))
                    .map(|job| (Pool::from(repository), job.clone())),
            );
        }
        let now = time::now_ms();
        for (pool, job) in seen {
            self.note_job(
                &pool,
                Attempt {
                    run_id: job.run_id,
                    run_attempt: job.run_attempt,
                },
                job.id,
                job.workflow.as_deref(),
                now,
            );
        }
    }

    /// Why `pool`'s next warm runner starts: as one of its configured warm runners while fewer of its runners wait
    /// with no queued job to take, else for a job its runs are expected to queue (`warm::target`). Its idle runners
    /// cover its queued jobs first, as the plan counts them.
    pub(super) fn warm_purpose(&self, pool: &Pool) -> Purpose {
        let Some(repository) = self
            .config
            .repositories
            .iter()
            .find(|repository| Pool::from(*repository) == *pool)
        else {
            return Purpose::Burst;
        };
        let configured = self.warm.learned().map_or(0, |learned| {
            warm::target(
                repository.warm,
                learned.active(&repository.name, time::now_ms()),
                0,
            )
        });
        let taken = self.taken();
        let queued = self
            .repositories
            .listed(&pool.repository)
            .map_or(0, |state| {
                state
                    .listing()
                    .jobs()
                    .iter()
                    .filter(|job| !taken.contains(&job.id) && matches(job, &repository.labels))
                    .count()
            });
        // A start in flight is a runner with no job yet, as the one it becomes is once placed.
        let waiting = self
            .slots
            .get(&pool.profile)
            .into_iter()
            .flatten()
            .filter(|slot| match slot.holder() {
                Holder::Free => false,
                Holder::Starting(starting) => starting.repository() == pool.repository,
                Holder::Runner(occupant) => {
                    occupant.repository == pool.repository && occupant.idle()
                }
            })
            .count();
        if waiting.saturating_sub(queued) < configured {
            Purpose::Warm
        } else {
            Purpose::Burst
        }
    }

    /// How many warm runners each pool wants now.
    pub(super) fn warm_targets(&self) -> BTreeMap<Pool, usize> {
        let Some(learned) = self.warm.learned() else {
            return BTreeMap::new();
        };
        let now = time::now_ms();
        self.config
            .repositories
            .iter()
            .map(|repository| {
                let pool = Pool::from(repository);
                let wanted = warm::target(
                    repository.warm,
                    learned.active(&repository.name, now),
                    self.warm.runs.remaining(&pool, learned, now),
                );
                (pool, wanted)
            })
            .collect()
    }
}
