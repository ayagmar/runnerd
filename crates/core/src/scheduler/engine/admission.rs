//! Admission: what a new runner reserves of the slice's budget. By caps, its profile's memory. Measured, the
//! largest predicted peak of its repository's queued jobs (`admission::Predictor`, from the job history, read at
//! most once a minute), since GitHub may give the runner any of them; the cap while the slice's memory has no
//! recent sample; and nothing starts while the slice is at `admission::HIGH_WATER` percent of its `MemoryMax`. A
//! job queued twice, queued or running beside a copy, or in a repository with a busy runner whose job is not known
//! yet, is predicted with `admission::Copies::Several`.
//! Under `[host] cpu_overcommit`, a new runner also reserves CPU: its profile's `cpus`, or measured the largest
//! predicted average CPU of its repository's queued jobs, then its own job's once known.
//! Under either policy a new runner also fits what the host's memory leaves the slice beside its other programs
//! (`admission::host`), and nothing starts while the host stalls for memory; running runners are never stopped for
//! it. The spans in which that held a job back go into the history, so sizing does not read them as waits for a
//! slot.
//! It reads the store and the latest host sample and reading; it never writes the config.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use super::Engine;
use crate::admission::{Copies, HIGH_WATER, Key, Prediction, Predictor, host};
use crate::api::{self, AdmissionPolicy, Basis};
use crate::config::Admission;
use crate::docker::DockerApi;
use crate::github::{GithubApi, QueuedJob};
use crate::observe::metrics::SAMPLE_INTERVAL;
use crate::scheduler::{Decision, Pool, Repository, matches};
use crate::sizing::{Measure, Ran};
use crate::time;

/// How often the predictor reads the job history again.
const REREAD: Duration = Duration::from_secs(60);

/// How far back it reads, in milliseconds: 30 days.
const HISTORY: u64 = 30 * 24 * 3600 * 1000;

/// What admission keeps between plans.
#[derive(Debug, Default)]
pub(super) struct Measured {
    /// What the last plan saw of the host's memory.
    pub host: host::Remembered,
    /// The jobs the last plan had waiting for the host's memory, and when (ms): each waited since then.
    host_waiting: (BTreeSet<u64>, u64),
    /// What predicts the jobs' peaks, and when it read the history.
    predictor: Option<(Predictor, SystemTime)>,
    /// Whether the slice was at the high-water mark at the last plan, so the change is logged once.
    pub high_water: bool,
    /// The waiting runners this tick's `rereserve` raised for a heavier queued job.
    raised: Vec<String>,
}

/// What one plan admits by.
#[derive(Debug, Clone)]
pub(super) struct Admitting {
    pub policy: AdmissionPolicy,
    /// What each pool's next runner reserves goes by; absent, its profile's memory and `default`.
    pub basis: BTreeMap<Pool, Basis>,
    /// What a runner reserving its profile's memory goes by.
    pub default: Basis,
    /// The budget left; `None` for an unlimited slice.
    pub free: Option<u64>,
    /// The slice's memory, when sampled recently.
    pub slice: Option<u64>,
    /// Whether the slice is at the high-water mark, so nothing starts.
    pub held: bool,
    /// What the host's memory leaves new runners, from a recent reading.
    pub host: Option<api::HostBudget>,
    /// Under `[host] cpu_overcommit`, what the plan admits by CPU.
    pub cpu: Option<CpuAdmitting>,
}

/// What one plan admits by CPU.
#[derive(Debug, Clone)]
pub(super) struct CpuAdmitting {
    /// What each pool's next runner reserves goes by; absent, its profile's `cpus`.
    pub basis: BTreeMap<Pool, Basis>,
    /// The CPU budget left, in millicores.
    pub free: u64,
}

impl Admitting {
    /// The record of a runner of `pool` started reserving `reserved` and `cpu` millicores, which takes them from
    /// the budgets left.
    pub(super) fn admit(&mut self, pool: &Pool, reserved: u64, cpu: u64) -> api::Admission {
        let admission = api::Admission {
            policy: self.policy,
            reserved,
            basis: self.basis.get(pool).copied().unwrap_or(self.default),
            free: self.free,
            slice: self.slice,
            cpu: self.cpu.as_mut().map(|admitting| {
                let admission = api::CpuAdmission {
                    reserved: cpu,
                    basis: admitting.basis.get(pool).copied().unwrap_or(Basis::Cap),
                    free: admitting.free,
                    waited: None,
                };
                admitting.free = admitting.free.saturating_sub(cpu);
                admission
            }),
        };
        self.free = self.free.map(|free| free.saturating_sub(reserved));
        admission
    }
}

/// What the runners run, as far as admission can tell copies of a job apart.
struct Running {
    /// The jobs runners are known to run.
    keys: BTreeSet<Key>,
    /// The repositories with a runner that took a job not known yet, which may be a copy of any of theirs.
    unknown: BTreeSet<String>,
}

impl Running {
    /// Whether a copy of `key` may be running.
    fn may_run(&self, key: &Key) -> bool {
        self.keys.contains(key) || self.unknown.contains(&key.repository)
    }
}

/// The largest prediction among `queued`, jobs of `repository` in `profile` under `cap`. A job queued twice, or
/// that may have a copy `running`, has copies beside it.
fn largest<'job>(
    predictor: &Predictor,
    repository: &str,
    profile: &str,
    cap: u64,
    queued: impl Iterator<Item = &'job QueuedJob>,
    running: &Running,
    now: u64,
) -> Option<Prediction> {
    let keys: Vec<Key> = queued
        .map(|job| Key {
            repository: repository.to_owned(),
            workflow: job.workflow.clone(),
            name: job.name.clone(),
        })
        .collect();
    keys.iter()
        .map(|key| {
            let queued = keys.iter().filter(|other| *other == key).count();
            let copies = if queued > 1 || running.may_run(key) {
                Copies::Several
            } else {
                Copies::Alone
            };
            predictor.predict(key, profile, cap, now, copies)
        })
        .max_by_key(|prediction| prediction.memory)
}

/// What a runner admitted by CPU (`reserved`) reserves once its job `key` in `profile`, whose `cpus` are `cap`
/// millicores, is known: that job's predicted average CPU, at most the whole `budget`.
fn job_cpu(
    predictor: &Predictor,
    key: &Key,
    profile: &str,
    cap: u64,
    budget: Option<u64>,
    reserved: api::CpuAdmission,
) -> api::CpuAdmission {
    let prediction = predictor.predict_cpu(key, profile, cap);
    api::CpuAdmission {
        reserved: budget.map_or(prediction.cpu, |budget| prediction.cpu.min(budget)),
        basis: prediction.basis,
        ..reserved
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
        let mut runs: Vec<(Ran, bool)> = history
            .jobs
            .into_iter()
            .map(|run| (run, true))
            .chain(history.cancelled.into_iter().map(|run| (run, false)))
            .collect();
        runs.sort_by_key(|(run, _)| run.finished_at.unwrap_or(run.queued_at));
        let mut predictor = Predictor::new(&self.config.sizing);
        for (run, finished) in &runs {
            let cap = run
                .memory_cap
                .or_else(|| self.config.profiles.get(&run.profile).map(|p| p.memory.0))
                .unwrap_or(u64::MAX);
            let at = run.finished_at.unwrap_or(run.queued_at);
            if run.measure == Measure::WithActiveFile {
                // Its peak counted page cache read twice, so neither it nor a peak near the cap says what the job
                // needs; a kill for memory still does, and its CPU is counted as ever.
                if *finished {
                    predictor.learn_cpu(run, &run.profile);
                }
                if run.oom_killed {
                    predictor.mark(
                        &Ran {
                            reserved: None,
                            ..run.clone()
                        },
                        &run.profile,
                        cap,
                        at,
                    );
                }
            } else if *finished {
                predictor.learn(run, &run.profile, cap, at);
            } else {
                // A cancelled run's peak may be cut short, so only how it ended counts.
                predictor.mark(run, &run.profile, cap, at);
            }
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
            host: self.host_budget(),
            cpu: self.admitting_cpu(repositories),
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
        let running = self.running();
        for repository in repositories {
            let Some(cap) = self
                .config
                .profiles
                .get(&repository.profile)
                .map(|p| p.memory.0)
            else {
                continue;
            };
            let queued = repository
                .queued
                .iter()
                .filter(|job| matches(job, &repository.labels));
            if let Some(prediction) = largest(
                predictor,
                &repository.name,
                &repository.profile,
                cap,
                queued,
                &running,
                now,
            ) {
                repository.runner_memory = Some(prediction.memory);
                admitting.basis.insert(repository.pool(), prediction.basis);
            }
        }
        admitting
    }

    /// Makes the budgets `admitting` leaves those the runners leave now, so a start's record says what was free
    /// when it started: a stop since the plan gave its runner's back, and a start that failed took nothing.
    pub(super) fn refresh_free(&self, admitting: &mut Admitting) {
        if !admitting.held {
            admitting.free = self
                .memory_max
                .map(|max| max.saturating_sub(self.memory_in_use()));
        }
        if let (Some(cpu), Some(budget)) = (admitting.cpu.as_mut(), self.cpu_budget()) {
            cpu.free = budget.saturating_sub(self.cpu_in_use());
        }
    }

    /// What the host's memory leaves runners beside what runs now, from a reading of the last few sample intervals.
    pub(super) fn host_budget(&self) -> Option<api::HostBudget> {
        let fresh = (3 * SAMPLE_INTERVAL).as_secs();
        let memory = self
            .store
            .latest_host_memory()
            .filter(|memory| memory.at + fresh >= time::now_ms() / 1000)?;
        // The larger margin keeps a start from pushing a busy host back to the edge; with no runner in the slice
        // there is no such start to follow, so the first runner needs only the margin.
        let remembered = host::Remembered {
            short: self.measured.host.short && self.occupied().next().is_some(),
            ..self.measured.host
        };
        Some(host::budget(memory, self.memory_in_use(), remembered))
    }

    /// Remembers what this plan saw of the host for the next: whether it stalled, and which jobs `waiting` wait for
    /// the host's memory. Adds the time since the last plan to the waits of those that waited then, and logs when
    /// the host becomes short and when it has room again.
    pub(super) fn note_host(
        &mut self,
        admitting: &Admitting,
        waiting: &BTreeMap<u64, api::Waiting>,
    ) {
        let stalled = admitting.host.as_ref().is_some_and(|host| host.stalled);
        let short = stalled
            || waiting
                .values()
                .any(|reason| matches!(reason, api::Waiting::HostMemory { .. }));
        let was_short = self.measured.host.short;
        self.measured.host = host::Remembered { short, stalled };
        let now = time::now_ms();
        let waiting_now: BTreeSet<u64> = waiting
            .iter()
            .filter(|(_, reason)| matches!(reason, api::Waiting::HostMemory { .. }))
            .map(|(job, _)| *job)
            .collect();
        let (waited, since) =
            std::mem::replace(&mut self.measured.host_waiting, (waiting_now, now));
        if !waited.is_empty() {
            self.store
                .host_waited(waited.into_iter().collect(), now.saturating_sub(since));
        }
        match (short, was_short) {
            (true, false) => {
                if let Some(host) = &admitting.host {
                    info!(
                        "the host is short of memory ({} available{}): no runner starts that does not fit {} \
                         beside what runs",
                        crate::config::ByteSize(host.memory.available),
                        if stalled {
                            ", its tasks stall for it"
                        } else {
                            ""
                        },
                        crate::config::ByteSize(host.limit)
                    );
                }
            }
            (false, true) => {
                info!("the host has memory to spare again: runners start as the budget allows");
            }
            _ => {}
        }
    }

    /// The CPU budget, in millicores: the slice's CPUs times `[host] cpu_overcommit`; `None` without it.
    pub(super) fn cpu_budget(&self) -> Option<u64> {
        let overcommit = self.config.host.cpu_overcommit?;
        Some(crate::config::cpu_budget(self.cpus?, overcommit))
    }

    /// What the runners reserve of the CPU budget, in millicores: under caps each its container's CPU limit, whatever
    /// it reserved under measured admission before a reload, as memory counts it.
    pub(super) fn cpu_in_use(&self) -> u64 {
        self.occupied()
            .map(|(_, occupant)| self.reserves_cpu(occupant))
            .sum()
    }

    /// What a runner reserves of the CPU budget, in millicores, as `cpu_in_use` counts it.
    pub(super) fn reserves_cpu(&self, occupant: &super::Occupant) -> u64 {
        match self.config.host.admission {
            Admission::Caps => occupant.cpus,
            Admission::Measured => occupant.reserved_cpu(),
        }
    }

    /// Under a CPU budget, sets what each repository's next runner reserves of it, at most the whole budget so a
    /// runner can always start alone: its profile's `cpus`, or under measured admission the largest predicted
    /// average CPU of its queued jobs. Says what the plan admits by CPU.
    fn admitting_cpu(&self, repositories: &mut [Repository]) -> Option<CpuAdmitting> {
        let budget = self.cpu_budget()?;
        let mut admitting = CpuAdmitting {
            basis: BTreeMap::new(),
            free: budget.saturating_sub(self.cpu_in_use()),
        };
        for repository in repositories {
            let Some(cap) = self
                .config
                .profiles
                .get(&repository.profile)
                .map(|p| u64::from(p.cpus) * 1000)
            else {
                continue;
            };
            let prediction = self.measured.predictor.as_ref().and_then(|(predictor, _)| {
                repository
                    .queued
                    .iter()
                    .filter(|job| matches(job, &repository.labels))
                    .map(|job| {
                        let key = Key {
                            repository: repository.name.clone(),
                            workflow: job.workflow.clone(),
                            name: job.name.clone(),
                        };
                        predictor.predict_cpu(&key, &repository.profile, cap)
                    })
                    .max_by_key(|prediction| prediction.cpu)
            });
            let (cpu, basis) = prediction.map_or((cap, Basis::Cap), |prediction| {
                (prediction.cpu, prediction.basis)
            });
            repository.runner_cpu = Some(cpu.min(budget));
            admitting.basis.insert(repository.pool(), basis);
        }
        Some(admitting)
    }

    /// Tells apart the jobs of the runners whose job the history now has a record of, unless that record is the
    /// unnamed one a runner's claim makes of a job no listing saw: GitHub's details name it (`follow_jobs`).
    async fn find_keys(&mut self) {
        let unknown: Vec<(String, u64)> = self
            .occupied()
            .filter(|(_, occupant)| occupant.key.is_none())
            .filter_map(|(_, occupant)| Some((occupant.runner_name.clone(), occupant.job?)))
            .collect();
        for (runner, job) in unknown {
            let Ok(Some(record)) = self.store.job(job).await else {
                continue;
            };
            if record.summary.name.is_empty() {
                continue;
            }
            if let Some(occupant) = self
                .slots
                .values_mut()
                .flatten()
                .flatten()
                .find(|occupant| occupant.runner_name == runner)
            {
                occupant.key = Some(Key {
                    repository: record.summary.repository,
                    workflow: record.summary.workflow,
                    name: record.summary.name,
                });
            }
        }
    }

    /// Under measured admission, sets what each running runner reserves: once its job is known, that job's
    /// prediction; until then, the larger of what it reserved and the largest prediction among its repository's
    /// queued jobs, since GitHub may still give it any of them. A runner a restart adopted without a record of its
    /// admission keeps its cap. Each change goes into the runner's row.
    pub(super) async fn rereserve(&mut self) {
        self.measured.raised.clear();
        if self.config.host.admission != Admission::Measured {
            return;
        }
        self.find_keys().await;
        let Some((predictor, _)) = &self.measured.predictor else {
            return;
        };
        let now = time::now_ms();
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        let running = self.running();
        let cpu_budget = self.cpu_budget();
        // The first runner of each known job, in slot order, goes by its own runs; its copies have it beside them,
        // as does every runner of a repository with a busy runner whose job is not known yet.
        let mut seen = BTreeSet::new();
        let mut changed = Vec::new();
        let mut raised = Vec::new();
        for (profile, slots) in &mut self.slots {
            for occupant in slots.iter_mut().flatten() {
                let first = occupant
                    .key
                    .as_ref()
                    .is_some_and(|key| seen.insert(key.clone()));
                let Some(admission) = &mut occupant.admission else {
                    continue;
                };
                // Its job's average CPU once known; until then what it started with, the largest of its queue's.
                let cpu = admission.cpu.zip(occupant.key.as_ref()).map(|(cpu, key)| {
                    job_cpu(predictor, key, profile, occupant.cpus, cpu_budget, cpu)
                });
                let mut dirty = cpu.is_some() && cpu != admission.cpu;
                admission.cpu = cpu.or(admission.cpu);
                let prediction = if let Some(key) = &occupant.key {
                    let copies = if first && !running.unknown.contains(&key.repository) {
                        Copies::Alone
                    } else {
                        Copies::Several
                    };
                    Some(predictor.predict(key, profile, occupant.memory, now, copies))
                } else {
                    let labels = self
                        .config
                        .pool_in(&occupant.repository, profile)
                        .map(|repository| repository.labels.as_slice())
                        .unwrap_or_default();
                    let queued = self
                        .repositories
                        .get(&occupant.repository)
                        .map(|state| state.queued.as_slice())
                        .unwrap_or_default()
                        .iter()
                        .filter(|job| !taken.contains(&job.id) && matches(job, labels));
                    largest(
                        predictor,
                        &occupant.repository,
                        profile,
                        occupant.memory,
                        queued,
                        &running,
                        now,
                    )
                    .filter(|prediction| prediction.memory > admission.reserved)
                };
                if let Some(prediction) = prediction
                    && (prediction.memory, prediction.basis)
                        != (admission.reserved, admission.basis)
                {
                    admission.reserved = prediction.memory;
                    admission.basis = prediction.basis;
                    dirty = true;
                    if occupant.key.is_none() {
                        raised.push(occupant.runner_name.clone());
                    }
                }
                if dirty {
                    changed.push((occupant.runner_name.clone(), *admission));
                }
            }
        }
        self.measured.raised = raised;
        for (runner, admission) in changed {
            self.store.runner_admission(&runner, admission);
        }
    }

    /// What the runners run: the jobs they are known to, and the repositories with a busy runner whose job is not
    /// known yet, as when its log capture failed.
    fn running(&self) -> Running {
        Running {
            keys: self
                .occupied()
                .filter_map(|(_, occupant)| occupant.key.clone())
                .collect(),
            unknown: self
                .occupied()
                .filter(|(_, occupant)| {
                    (occupant.busy || occupant.job.is_some()) && occupant.key.is_none()
                })
                .map(|(_, occupant)| occupant.repository.clone())
                .collect(),
        }
    }

    /// The waiting runners to stop, as one that waited `IDLE_TIMEOUT` is: those `rereserve` raised for a heavier
    /// queued job while what the running containers take is past `MemoryMax`, until it fits again. GitHub could give
    /// them that job, so it waits for a runner that fits. One assigned between two ticks keeps it, and counts at
    /// its raised reservation from then.
    pub(super) fn over_budget(&self, decisions: &[Decision]) -> Vec<Decision> {
        let Some(max) = self.memory_max else {
            return Vec::new();
        };
        let mut in_use = self.memory_in_use();
        let mut stops = Vec::new();
        for (slot, occupant) in self.occupied() {
            if in_use <= max {
                break;
            }
            let stopping = decisions.iter().any(
                |decision| matches!(decision, Decision::Stop { slot: other } if *other == slot),
            );
            if occupant.busy
                || occupant.job.is_some()
                || stopping
                || !self.measured.raised.contains(&occupant.runner_name)
            {
                continue;
            }
            info!(
                "{}: stopping {}: raised to {} for a queued job, it does not fit the budget",
                occupant.repository,
                occupant.runner_name,
                crate::config::ByteSize(occupant.reserved())
            );
            in_use = in_use.saturating_sub(self.takes(occupant));
            stops.push(Decision::Stop { slot });
        }
        stops
    }

    /// Notes which pools this plan held for CPU (`held`). A held pool without a wait starts one for its oldest
    /// queued job, logging it; a wait ends when its runner starts or, once a listing succeeded, its job is no
    /// longer queued, not when a later plan holds the job for memory or a slot instead.
    pub(super) fn note_held_for_cpu(&mut self, held: &BTreeSet<Pool>) {
        let budget = self.cpu_budget();
        let in_use = self.cpu_in_use();
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        let mut idle: BTreeMap<Pool, usize> = BTreeMap::new();
        for (slot, occupant) in self.occupied() {
            if !occupant.busy && occupant.job.is_none() {
                let pool = Pool {
                    repository: occupant.repository.clone(),
                    profile: slot.profile,
                };
                *idle.entry(pool).or_default() += 1;
            }
        }
        for repository in &self.config.repositories {
            let name = &repository.name;
            let pool = Pool::from(repository);
            let Some(state) = self.repositories.get_mut(name) else {
                continue;
            };
            let queued = state
                .queued
                .iter()
                .filter(|job| !taken.contains(&job.id) && matches(job, &repository.labels));
            if state.listed
                && let Some(wait) = state.cpu_wait.get(&repository.profile)
                && !queued.clone().any(|job| job.id == wait.job)
            {
                state.cpu_wait.remove(&repository.profile);
            }
            let cpu_held = held.contains(&pool);
            if cpu_held {
                state.cpu_held.insert(repository.profile.clone());
            } else {
                state.cpu_held.remove(&repository.profile);
            }
            // As `plan` serves them: its waiting runners take the oldest jobs, the next one is held.
            let mut ids: Vec<u64> = queued.map(|job| job.id).collect();
            ids.sort_unstable();
            if cpu_held
                && !state.cpu_wait.contains_key(&repository.profile)
                && let Some(oldest) = ids.get(idle.get(&pool).copied().unwrap_or(0)).copied()
            {
                state.cpu_wait.insert(
                    repository.profile.clone(),
                    super::CpuWait {
                        job: oldest,
                        since: SystemTime::now(),
                    },
                );
                if let Some(budget) = budget {
                    info!(
                        "{name}: its next job in {} waits for CPU: runners reserve {} of the budget's {}",
                        repository.profile,
                        api::cpus(in_use),
                        api::cpus(budget)
                    );
                }
            }
        }
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
