//! Admission: what a new runner reserves of the slice's budget. By caps, its profile's memory. Measured, the
//! largest predicted peak of its repository's queued jobs (`admission::Predictor`, from the job history, read at
//! most once a minute), since GitHub may give the runner any of them; the cap while the slice's memory has no
//! recent sample; and nothing starts while the slice is at `admission::HIGH_WATER` percent of its `MemoryMax`.
//! It reads the store and the latest host sample; it never writes the config.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use super::Engine;
use crate::admission::{HIGH_WATER, Key, Predictor};
use crate::api::{self, AdmissionPolicy, Basis};
use crate::config::Admission;
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::observe::metrics::SAMPLE_INTERVAL;
use crate::scheduler::Repository;
use crate::time;

/// How often the predictor reads the job history again.
const REREAD: Duration = Duration::from_secs(60);

/// How far back it reads, in milliseconds: 30 days.
const HISTORY: u64 = 30 * 24 * 3600 * 1000;

/// What measured admission keeps between plans.
#[derive(Debug, Default)]
pub(super) struct Measured {
    /// What predicts the jobs' peaks, and when it read the history.
    predictor: Option<(Predictor, SystemTime)>,
    /// Whether the slice was at the high-water mark at the last plan, so the change is logged once.
    pub high_water: bool,
}

/// What one plan admits by.
#[derive(Debug, Clone)]
pub(super) struct Admitting {
    pub policy: AdmissionPolicy,
    /// What each repository's next runner reserves goes by; absent, its profile's memory and `default`.
    pub basis: BTreeMap<String, Basis>,
    /// What a runner reserving its profile's memory goes by.
    pub default: Basis,
    /// The budget left; `None` for an unlimited slice.
    pub free: Option<u64>,
    /// The slice's memory, when sampled recently.
    pub slice: Option<u64>,
    /// Whether the slice is at the high-water mark, so nothing starts.
    pub held: bool,
}

impl Admitting {
    /// The record of a runner of `repository` started reserving `reserved`, which takes it from the budget left.
    pub(super) fn admit(&mut self, repository: &str, reserved: u64) -> api::Admission {
        let admission = api::Admission {
            policy: self.policy,
            reserved,
            basis: self.basis.get(repository).copied().unwrap_or(self.default),
            free: self.free,
            slice: self.slice,
        };
        self.free = self.free.map(|free| free.saturating_sub(reserved));
        admission
    }
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Under measured admission, reads the job history into a new predictor once the last is a minute old. A read
    /// that fails keeps the last one; without any, jobs are admitted at their caps.
    pub(super) async fn refresh_predictor(&mut self) {
        if self.config.host.admission != Admission::Measured {
            self.measured.predictor = None;
            return;
        }
        if self
            .measured
            .predictor
            .as_ref()
            .is_some_and(|(_, read_at)| read_at.elapsed().is_ok_and(|age| age < REREAD))
        {
            return;
        }
        let history = match self
            .store
            .sizing(time::now_ms().saturating_sub(HISTORY))
            .await
        {
            Ok(history) => history,
            Err(error) => {
                warn!("cannot read the job history to predict peaks: {error}");
                return;
            }
        };
        let mut jobs = history.jobs;
        jobs.sort_by_key(|run| run.finished_at.unwrap_or(run.queued_at));
        let mut predictor = Predictor::new(&self.config.sizing);
        for run in &jobs {
            let cap = run
                .memory_cap
                .or_else(|| self.config.profiles.get(&run.profile).map(|p| p.memory.0))
                .unwrap_or(u64::MAX);
            predictor.learn(
                run,
                &run.profile,
                cap,
                run.finished_at.unwrap_or(run.queued_at),
            );
        }
        self.measured.predictor = Some((predictor, SystemTime::now()));
    }

    /// Sets what each repository's next runner reserves, and says what the plan admits by. `free` is the budget
    /// the running containers leave.
    pub(super) fn admitting(
        &self,
        repositories: &mut [Repository],
        free: Option<u64>,
    ) -> Admitting {
        let mut admitting = Admitting {
            policy: AdmissionPolicy::Caps,
            basis: BTreeMap::new(),
            default: Basis::Cap,
            free,
            slice: None,
            held: false,
        };
        if self.config.host.admission != Admission::Measured {
            return admitting;
        }
        admitting.policy = AdmissionPolicy::Measured;
        let now = time::now_ms();
        // A sample from the last few intervals: the sampler may be a little late, never minutes.
        let fresh = (3 * SAMPLE_INTERVAL).as_secs();
        admitting.slice = self
            .store
            .latest_host_sample()
            .filter(|sample| sample.at + fresh >= now / 1000)
            .map(|sample| sample.memory);
        let (Some(slice), Some(memory_max)) = (admitting.slice, self.memory_max) else {
            admitting.default = Basis::Unmeasured;
            return admitting;
        };
        if slice.saturating_mul(100) >= memory_max.saturating_mul(HIGH_WATER) {
            admitting.held = true;
            admitting.free = Some(0);
            return admitting;
        }
        let Some((predictor, _)) = &self.measured.predictor else {
            return admitting;
        };
        for repository in repositories {
            let Some(cap) = self
                .config
                .profiles
                .get(&repository.profile)
                .map(|p| p.memory.0)
            else {
                continue;
            };
            let largest = repository
                .queued
                .iter()
                .map(|job| {
                    let key = Key {
                        repository: repository.name.clone(),
                        workflow: job.workflow.clone(),
                        name: job.name.clone(),
                    };
                    predictor.predict(&key, &repository.profile, cap, now)
                })
                .max_by_key(|prediction| prediction.memory);
            if let Some(prediction) = largest {
                repository.runner_memory = Some(prediction.memory);
                admitting
                    .basis
                    .insert(repository.name.clone(), prediction.basis);
            }
        }
        admitting
    }

    /// Logs when the slice reaches the high-water mark and when it falls under it.
    pub(super) fn note_high_water(&mut self, admitting: &Admitting) {
        if admitting.held == self.measured.high_water {
            return;
        }
        self.measured.high_water = admitting.held;
        let slice = admitting.slice.map_or_else(String::new, |memory| {
            format!(" ({})", crate::config::ByteSize(memory))
        });
        if admitting.held {
            info!(
                "the slice{slice} is at {HIGH_WATER}% of its MemoryMax: measured admission starts nothing until it \
                 falls under"
            );
        } else {
            info!(
                "the slice{slice} is under {HIGH_WATER}% of its MemoryMax again: measured admission resumes"
            );
        }
    }
}
