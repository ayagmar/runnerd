//! Warm runners (ADR 0050): reads what `scheduler::warm` learns from the job history every few minutes, follows the
//! workflow runs whose jobs it sees queued or taken, and says how many warm runners each repository wants. It must not
//! start or stop runners: `plan` decides with what it says.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use tracing::warn;

use super::{Engine, Purpose};
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::scheduler::warm::{self, Attempt, Learned, Runs};
use crate::scheduler::{Pool, matches};
use crate::time;

/// How often the history is read again.
const REREAD: Duration = Duration::from_secs(300);

/// What warm runners keep between ticks.
#[derive(Debug, Default)]
pub(super) struct Warm {
    /// What the history taught, and when it was read; `None` before the first read, when no repository wants any.
    pub(super) learned: Option<(Learned, SystemTime)>,
    /// The workflow runs seen lately.
    runs: Runs,
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Reads the history again once the last read is `REREAD` old. A read that fails keeps the last one.
    pub(super) async fn refresh_warm(&mut self) {
        if self
            .warm
            .learned
            .as_ref()
            .is_some_and(|(_, read_at)| read_at.elapsed().is_ok_and(|age| age < REREAD))
        {
            return;
        }
        let now = time::now_ms();
        match self.store.warm(now.saturating_sub(warm::LOOKBACK)).await {
            Ok(seen) => {
                let config = &self.config;
                let mut learned = Learned::new(&seen, now, |seen| {
                    config
                        .pool(&seen.repository, &seen.labels)
                        .map(|pool| pool.profile.clone())
                });
                if let Some((older, _)) = &self.warm.learned {
                    learned.absorb(older);
                }
                self.warm.learned = Some((learned, SystemTime::now()));
            }
            Err(error) => warn!("cannot read the job history for warm runners: {error}"),
        }
        self.warm.runs.expire(now);
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
        if let Some((learned, _)) = &mut self.warm.learned {
            learned.note(&pool.repository, at);
        }
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
            let Some(state) = self.repositories.get(&repository.name) else {
                continue;
            };
            seen.extend(
                state
                    .queued
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
        let configured = self.warm.learned.as_ref().map_or(0, |(learned, _)| {
            warm::target(
                repository.warm,
                learned.active(&repository.name, time::now_ms()),
                0,
            )
        });
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        let queued = self.repositories.get(&pool.repository).map_or(0, |state| {
            state
                .queued
                .iter()
                .filter(|job| !taken.contains(&job.id) && matches(job, &repository.labels))
                .count()
        });
        let waiting = self
            .occupied()
            .filter(|(slot, occupant)| {
                slot.profile == pool.profile
                    && occupant.repository == pool.repository
                    && !occupant.busy
                    && occupant.job.is_none()
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
        let Some((learned, _)) = &self.warm.learned else {
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
