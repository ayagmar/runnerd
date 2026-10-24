//! Warm runners (ADR 0050): reads what `scheduler::warm` learns from the job history every few minutes, follows the
//! workflow runs whose jobs it sees queued or taken, and says how many warm runners each repository wants. It must not
//! start or stop runners: `plan` decides with what it says.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use tracing::warn;

use super::Engine;
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::scheduler::matches;
use crate::scheduler::warm::{self, Learned, Runs};
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
                let mut learned = Learned::new(&seen, now);
                if let Some((older, _)) = &self.warm.learned {
                    learned.absorb(older);
                }
                self.warm.learned = Some((learned, SystemTime::now()));
            }
            Err(error) => warn!("cannot read the job history for warm runners: {error}"),
        }
        if let Some((learned, _)) = &self.warm.learned {
            self.warm.runs.expire(learned, now);
        }
    }

    /// Notes a job of `repository` seen queued or taken now, of run `run_id`, with its workflow when known.
    pub(super) fn note_job(
        &mut self,
        repository: &str,
        run_id: u64,
        job: u64,
        workflow: Option<&str>,
    ) {
        if let Some((learned, _)) = &mut self.warm.learned {
            learned.note(repository, time::now_ms());
        }
        self.warm
            .runs
            .note(repository, run_id, job, workflow, time::now_ms());
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
                    .map(|job| (repository.name.clone(), job.clone())),
            );
        }
        for (repository, job) in seen {
            self.note_job(&repository, job.run_id, job.id, job.workflow.as_deref());
        }
    }

    /// How many warm runners each repository wants now, by name.
    pub(super) fn warm_targets(&self) -> BTreeMap<String, usize> {
        let Some((learned, _)) = &self.warm.learned else {
            return BTreeMap::new();
        };
        let now = time::now_ms();
        self.config
            .repositories
            .iter()
            .map(|repository| {
                let wanted = warm::target(
                    repository.warm,
                    learned.active(&repository.name, now),
                    self.warm.runs.remaining(&repository.name, learned, now),
                );
                (repository.name.clone(), wanted)
            })
            .collect()
    }
}
