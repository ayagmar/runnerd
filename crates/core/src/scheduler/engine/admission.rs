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
//! Under either policy the same predictor gives each pool and each running runner its usual run time, which says what
//! may start behind a barrier (ADR 0097).
//! Measured, what runs counts as `admission::pooled` adds it up (ADR 0099, 0130), its growth brake one of what admission
//! keeps between plans.
//! It reads the store and the latest host sample and reading; it never writes the config.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use super::open_waits::{OpenWaits, WaitFor, Waited};
use super::{Engine, Slot};
use crate::admission::{Copies, HIGH_WATER, Key, Prediction, Predictor, growth, host};
use crate::api::{self, AdmissionPolicy, Basis};
use crate::config::Admission;
use crate::docker::DockerApi;
use crate::github::{GithubApi, QueuedJob};
use crate::observe::metrics::SAMPLE_INTERVAL;
use crate::scheduler::{
    Barrier, Decision, Demand, Plan, Pool, Repository, Scarce, StartFor, StopReason, matches,
};
use crate::time;

/// How often the predictor reads the job history again.
const REREAD: Duration = Duration::from_secs(60);

/// How far back it reads, in milliseconds: 30 days.
const HISTORY: u64 = 30 * 24 * 3600 * 1000;

/// What admission keeps between plans. Its fields are independent, each written only by its own method here.
///
/// Nothing of it survives a restart: the first plan reads the host and the history afresh, and the jobs it finds
/// waiting start their waits then; the growth count's brake starts held (`growth::Growth::restarted`). A reload keeps
/// it all; the next `rereserve` drops the raises if the reload turned measured admission off, and a reload that turns
/// the growth count on or changes what its guard reads begins its brake again (`restart_growth`).
#[derive(Debug)]
pub(super) struct Measured {
    /// What the last plan saw of the host's memory.
    host: host::HostBand,
    /// The jobs the last plans left waiting for a slot alone or for the host's memory.
    job_waits: OpenWaits,
    /// What predicts the jobs' peaks and usual run times, and when it read the history.
    predictor: Option<(Predictor, SystemTime)>,
    /// Whether the slice was at the high-water mark at the last plan, so the change is logged once.
    high_water: bool,
    /// The waiting runners `rereserve` raised for a heavier queued job, until they take a job or go. A raise stays
    /// once made, so a pass's raise is still known to the tick after it, which stops those past the budget.
    raised: Vec<String>,
    /// The job the last plan let no younger job start before, so the change is logged once.
    barrier: Option<u64>,
    /// What the host's limit held at the last plan, so the change is logged once.
    host_holds: HostHolds,
    /// The growth count's brake, its looks by runner name (ADR 0130).
    growth: growth::Growth<String>,
}

impl Measured {
    /// What a daemon starts with at `now`: nothing known, and the growth count held until it has a window of looks.
    pub(super) fn new(now: SystemTime) -> Self {
        Self {
            host: host::HostBand::default(),
            job_waits: OpenWaits::default(),
            predictor: None,
            high_water: false,
            raised: Vec::new(),
            barrier: None,
            host_holds: HostHolds::default(),
            growth: growth::Growth::restarted(now),
        }
    }

    /// The growth count's brake in force at `now`, if any.
    pub(super) fn braked(&self, now: SystemTime) -> Option<growth::Braked> {
        self.growth.braked(now)
    }

    /// A tick's look at what each runner `used`, the `room` the growth count keeps and whether it was above its
    /// `guard`'s line, at `now`.
    pub(super) fn look_at_growth(
        &mut self,
        now: SystemTime,
        used: Vec<growth::Use<String>>,
        room: u64,
        guard: growth::Guard,
    ) {
        self.growth.look(now, used, room, guard);
    }

    /// The growth count's looks were taken by other rules, or none were taken: it is held as after a restart at `now`.
    pub(super) fn restart_growth(&mut self, now: SystemTime) {
        self.growth = growth::Growth::restarted(now);
    }

    /// The growth count's looks and brake, `by` older.
    #[cfg(test)]
    pub(super) fn age_growth(&mut self, by: Duration) {
        self.growth.age(by);
    }

    /// What the last plan saw of the host's memory.
    pub(super) fn host(&self) -> host::HostBand {
        self.host
    }

    /// The band this plan saw, for the next; the last plan's.
    fn set_host(&mut self, band: host::HostBand) -> host::HostBand {
        std::mem::replace(&mut self.host, band)
    }

    /// What the host's limit holds back at this plan; what it held at the last.
    fn set_host_holds(&mut self, holds: HostHolds) -> HostHolds {
        std::mem::replace(&mut self.host_holds, holds)
    }

    /// The jobs the last plans left waiting for `kind`.
    pub(super) fn waiting(&self, kind: WaitFor) -> BTreeSet<u64> {
        self.job_waits.jobs(kind)
    }

    /// The jobs a plan has waiting for `kind` now: what ended or went on since the last commit.
    fn open_wait(&mut self, kind: WaitFor, waiting: BTreeSet<u64>, now: u64) -> Vec<Waited> {
        self.job_waits.open_wait(kind, waiting, now)
    }

    /// `job`'s runner is in its slot: its wait, if any, ended now.
    fn end_wait(&mut self, job: u64, now: u64) -> Option<Waited> {
        self.job_waits.end_wait(job, now)
    }

    /// What predicts the jobs' peaks and usual run times, once the history was read.
    fn predictor(&self) -> Option<&Predictor> {
        self.predictor.as_ref().map(|(predictor, _)| predictor)
    }

    /// Whether the predictor was read less than `REREAD` ago.
    fn predictor_is_fresh(&self) -> bool {
        self.predictor
            .as_ref()
            .is_some_and(|(_, read_at)| read_at.elapsed().is_ok_and(|age| age < REREAD))
    }

    /// The history was read into `predictor` at `read_at`.
    fn read_predictor(&mut self, predictor: Predictor, read_at: SystemTime) {
        self.predictor = Some((predictor, read_at));
    }

    /// Whether the slice was at the high-water mark at the last plan.
    pub(super) fn high_water(&self) -> bool {
        self.high_water
    }

    /// Whether the slice is at the high-water mark at this plan; whether that changed.
    fn set_high_water(&mut self, held: bool) -> bool {
        std::mem::replace(&mut self.high_water, held) != held
    }

    /// The job this plan lets no younger job start before; whether that changed.
    fn set_barrier(&mut self, job: Option<u64>) -> bool {
        std::mem::replace(&mut self.barrier, job) != job
    }

    /// Whether `rereserve` raised `runner` and it has not taken a job or gone since.
    fn is_raised(&self, runner: &str) -> bool {
        self.raised.iter().any(|name| name == runner)
    }

    /// The raised runners, for `rereserve` to update and give back.
    fn take_raised(&mut self) -> Vec<String> {
        std::mem::take(&mut self.raised)
    }

    /// The raised runners `rereserve` updated.
    fn keep_raised(&mut self, raised: Vec<String>) {
        self.raised = raised;
    }

    /// Measured admission is off: no runner is raised.
    fn forget_raised(&mut self) {
        self.raised.clear();
    }
}

#[cfg(test)]
impl Measured {
    pub(super) fn set_host_short(&mut self) {
        if self.host == host::HostBand::Plain {
            self.host = host::HostBand::Larger;
        }
    }

    pub(super) fn wait_for_the_host(&mut self, job: u64) {
        let mut waiting = self.job_waits.jobs(WaitFor::Host);
        waiting.insert(job);
        self.job_waits
            .open_wait(WaitFor::Host, waiting, time::now_ms());
    }

    pub(super) fn raise(&mut self, runner: String) {
        self.raised.push(runner);
    }

    pub(super) fn raised(&self) -> &[String] {
        &self.raised
    }

    pub(super) fn set_predictor(&mut self, predictor: Option<(Predictor, SystemTime)>) {
        self.predictor = predictor;
    }
}

/// What the host's limit holds back at a plan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum HostHolds {
    /// Nothing that keeps the larger margin up.
    #[default]
    Nothing,
    /// What runs reaches the limit, while the host has the room (`HostBudget::short_of`).
    Counted,
    /// The host stalls, or lacks what a job needs.
    Short,
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
    /// The margin `host` keeps, as the plan's starts each go by it.
    pub remembered: host::Remembered,
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
    /// Reads the job history into a new predictor once the last is a minute old. A read that fails keeps the last one;
    /// without any, jobs are admitted at their caps and nothing starts behind a barrier. Under caps only usual run times
    /// are read from it (ADR 0097).
    pub(super) async fn refresh_predictor(&mut self) {
        if self.measured.predictor_is_fresh() {
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
        let profiles = &self.config.profiles;
        let predictor = Predictor::read(&self.config.sizing, &history, |profile| {
            profiles.get(profile).map(|limits| limits.memory.0)
        });
        self.measured.read_predictor(predictor, SystemTime::now());
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
            remembered: self.host_remembered(),
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
            .filter(|sample| sample.at + fresh >= self.clock.now_ms() / 1000)
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
        let Some(predictor) = self.measured.predictor() else {
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
                .map(|max| max.saturating_sub(self.count(admitting.remembered).pooled()));
        }
        if let (Some(cpu), Some(budget)) = (admitting.cpu.as_mut(), self.cpu_budget()) {
            cpu.free = budget.saturating_sub(self.cpu_in_use());
        }
    }

    /// The host's latest memory reading, if it is from the last few sample intervals, with the config's
    /// `[host] memory_ceiling` (ADR 0084), so a reload that changes it applies at the next plan.
    pub(super) fn host_memory(&self) -> Option<api::HostMemory> {
        let fresh = (3 * SAMPLE_INTERVAL).as_secs();
        self.store
            .latest_host_memory()
            .filter(|memory| memory.at + fresh >= self.clock.now_ms() / 1000)
            .map(|memory| api::HostMemory {
                ceiling: self.config.host.memory_ceiling.map(|ceiling| ceiling.0),
                ..memory
            })
    }

    /// What the host's memory leaves runners beside what runs now, from a reading of the last few sample intervals.
    pub(super) fn host_budget(&self) -> Option<api::HostBudget> {
        self.host_budget_by(self.host_remembered())
    }

    /// What the host's memory leaves runners beside what runs now, from a reading of the last few sample intervals,
    /// with the margin `remembered` asks for.
    pub(super) fn host_budget_by(&self, remembered: host::Remembered) -> Option<api::HostBudget> {
        let memory = self.host_memory()?;
        Some(host::budget(memory, self.count(remembered), remembered))
    }

    /// What the last plan saw of the host, as the next one goes by it. The larger margin keeps a start from pushing a
    /// busy host back to the edge; with no runner in the slice there is no such start to follow, so the first
    /// runners need only the margin.
    pub(super) fn host_remembered(&self) -> host::Remembered {
        let occupancy = host::Occupancy::of(self.footprints().next().is_some());
        host::Remembered::new(self.measured.host(), occupancy)
    }

    /// Remembers what `plan` saw of the host for the next: whether it stalled, whether the larger margin stands
    /// (`host::short`), and which jobs wait for the host's memory. Adds the time since the last plan to the waits of
    /// those that waited then, tells insights which of them the host lacks room for (ADR 0103), and logs what holds
    /// them when that changes.
    pub(super) fn note_host(&mut self, admitting: &Admitting, plan: &Plan) {
        let stalled = admitting.host.as_ref().is_some_and(|host| host.stalled);
        // A job the plan starts without stopping a warm runner, as the tick's own starts did not, is a waiter the band
        // reads too: one that fits, whose start the larger margin must not hold back (ADR 0102). One that replaces warm
        // runners fits only once they go, which the plain room beside what runs does not show: it is no such waiter.
        let starts = plan.decisions.iter().filter_map(|decision| match decision {
            Decision::Start {
                memory,
                for_: StartFor::Job(_),
                replacing,
                ..
            } if replacing.is_empty() => Some(*memory),
            Decision::Start { .. } | Decision::Stop { .. } => None,
        });
        let needs = plan
            .waiting
            .values()
            .filter_map(|reason| match reason {
                api::Waiting::HostMemory { needs, .. } => Some(*needs),
                _ => None,
            })
            .chain(starts);
        let count = self.count(self.host_remembered());
        let band = admitting
            .host
            .as_ref()
            .map_or(host::HostBand::Plain, |host| {
                host::HostBand::after_plan(&host.memory, count.hosted(), stalled, needs)
            });
        let was_short = self.measured.set_host(band).is_short();
        let now = time::now_ms();
        let waiting_now: BTreeMap<u64, u64> = plan
            .waiting
            .iter()
            .filter_map(|(job, reason)| match reason {
                api::Waiting::HostMemory { needs, .. } => Some((*job, *needs)),
                _ => None,
            })
            .collect();
        let none_waiting = waiting_now.is_empty();
        // Insights tell the owner to close programs, so they hear only of the jobs the host itself lacks room for.
        // Every job the limit held still adds to `host_waited`: sizing takes that off the wait it reads as one for
        // the budget.
        let lacking: BTreeSet<u64> = admitting
            .host
            .iter()
            .flat_map(|host| {
                waiting_now
                    .iter()
                    .filter(|(_, needs)| host.short_of(**needs))
                    .map(|(job, _)| *job)
            })
            .collect();
        let holds = match (band.is_short(), stalled || !lacking.is_empty()) {
            (false, _) => HostHolds::Nothing,
            (true, false) => HostHolds::Counted,
            (true, true) => HostHolds::Short,
        };
        let held = self.measured.set_host_holds(holds);
        self.store.host_waiting(lacking, now);
        self.note_host_waits(waiting_now.into_keys().collect(), now);
        let size = crate::config::ByteSize;
        match (holds, admitting.host.as_ref()) {
            (HostHolds::Short, Some(host)) if held != HostHolds::Short => {
                info!(
                    "the host is short of memory ({} available{}): no runner starts that does not fit {} beside what \
                     runs",
                    size(host.memory.available),
                    if stalled {
                        ", its tasks stall for it"
                    } else {
                        ""
                    },
                    size(host.limit)
                );
            }
            (HostHolds::Counted, Some(host)) if held != HostHolds::Counted => {
                let (verb, growth) = match host.growth_clause(count.plain) {
                    Some(growth) => ("counts", format!(": {growth};")),
                    None => ("reserves", ":".to_owned()),
                };
                info!(
                    "what runs {verb} {} of the {} the host leaves the slice, with {} available{growth} no runner \
                     starts that does not fit beside it",
                    size(host.counts(count.plain)),
                    size(host.limit),
                    size(host.memory.available)
                );
            }
            (HostHolds::Nothing, _) if was_short && none_waiting => {
                info!("no job waits for the host's memory now: runners start as the budget allows");
            }
            (HostHolds::Nothing, _) if was_short => {
                info!(
                    "a job waiting for the host's memory fits its plain margin: the next plan goes by that margin"
                );
            }
            _ => {}
        }
    }

    /// Remembers which jobs a plan has waiting for the host's memory, and adds the time since the last commit to the
    /// host waits of those that did then (ADR 0103).
    pub(super) fn note_host_waits(&mut self, waiting: BTreeSet<u64>, now: u64) {
        let waited = self.measured.open_wait(WaitFor::Host, waiting, now);
        self.record_waited(waited);
    }

    /// Remembers which jobs this plan had waiting only for a slot (`Plan::slot_waiting`), and adds the time since the
    /// last commit to the slot waits of those that did then: what auto sizing reads to add slots (ADR 0090). A job a
    /// restart left waiting counts again from the first plan after it.
    pub(super) fn note_slots(&mut self, waiting: BTreeSet<u64>) {
        let waited = self
            .measured
            .open_wait(WaitFor::Slot, waiting, time::now_ms());
        self.record_waited(waited);
    }

    /// Commits the waits of a plan a pass carries out, as a tick commits its own: the jobs it has waiting for a slot
    /// and for the host's memory start or go on waiting now, not at the next tick (ADR 0110). The host's band and
    /// what insights hear stay the tick's. A job in `not_carried_out`, whose planned start the pass did not make, stays
    /// in the wait it was in.
    pub(super) fn note_pass_waits(&mut self, plan: &Plan, not_carried_out: &BTreeSet<u64>) {
        let still = |kind: WaitFor| -> BTreeSet<u64> {
            let waiting = self.measured.waiting(kind);
            waiting.intersection(not_carried_out).copied().collect()
        };
        let mut slots = plan.slot_waiting.clone();
        slots.extend(still(WaitFor::Slot));
        let mut host: BTreeSet<u64> = plan
            .waiting
            .iter()
            .filter(|(_, reason)| matches!(reason, api::Waiting::HostMemory { .. }))
            .map(|(job, _)| *job)
            .collect();
        host.extend(still(WaitFor::Host));
        self.note_slots(slots);
        self.note_host_waits(host, time::now_ms());
    }

    /// Ends `job`'s slot or host wait, as its runner is in its slot: it waited from the last commit until now, not
    /// until the next commit (ADR 0110).
    pub(super) fn note_started(&mut self, job: u64) {
        let waited = self.measured.end_wait(job, time::now_ms());
        self.record_waited(waited.into_iter().collect());
    }

    /// Adds what ended or went on to the history's waits.
    fn record_waited(&self, waited: Vec<Waited>) {
        for Waited { kind, jobs, ms } in waited {
            match kind {
                WaitFor::Slot => self.store.slot_waited(jobs, ms),
                WaitFor::Host => self.store.host_waited(jobs, ms),
            }
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
        self.footprints()
            .map(|footprint| self.reserves_cpu(&footprint))
            .sum()
    }

    /// What a runner, or a start in flight as the runner it becomes, reserves of the CPU budget, in millicores, as
    /// `cpu_in_use` counts it.
    pub(super) fn reserves_cpu(&self, occupant: &super::Footprint<'_>) -> u64 {
        match self.config.host.admission {
            Admission::Caps => occupant.cpus,
            Admission::Measured => occupant.reserved_cpu,
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
            let predictor = self
                .measured
                .predictor()
                .filter(|_| self.config.host.admission == Admission::Measured);
            let prediction = predictor.and_then(|predictor| {
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

    /// Sets how long a new runner of each of `repositories` may run: the longest usual run time of the queued jobs its
    /// labels match, `taken` left out (ADR 0097). Not `listed_demand`: a job it hides for a runner no capture named yet
    /// may still be queued, and GitHub may give it to the new runner.
    pub(super) fn usual_runs(&self, repositories: &mut [Repository], taken: &BTreeSet<u64>) {
        let Some(predictor) = self.measured.predictor() else {
            return;
        };
        for repository in repositories {
            let listed = self.listed_unnamed(&repository.name, taken);
            let queued = listed.iter().filter(|job| matches(job, &repository.labels));
            let keys = queued.map(|job| Key {
                repository: repository.name.clone(),
                workflow: job.workflow.clone(),
                name: job.name.clone(),
            });
            repository.usual = predictor.longest_usual(keys, &repository.profile);
        }
    }

    /// How long `occupant`'s job in `profile` usually keeps it, once that job is known and its runs say.
    pub(super) fn usual_run(&self, occupant: &super::Occupant, profile: &str) -> Option<Duration> {
        let predictor = self.measured.predictor()?;
        predictor.usual(occupant.key()?, profile)
    }

    /// Tells apart the jobs of the runners whose job the history now has a record of, unless that record is the
    /// unnamed one a runner's claim makes of a job no listing saw: GitHub's details name it (`follow_jobs`).
    pub(super) async fn find_keys(&mut self) {
        let unknown: Vec<(String, u64)> = self
            .occupied()
            .filter(|(_, occupant)| !occupant.has_key())
            .filter_map(|(_, occupant)| Some((occupant.runner_name.clone(), occupant.named_job()?)))
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
                .filter_map(Slot::occupant_mut)
                .find(|occupant| occupant.runner_name == runner)
            {
                occupant.engagement.keyed(Key {
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
        self.find_keys().await;
        if self.config.host.admission != Admission::Measured {
            self.measured.forget_raised();
            return;
        }
        let mut raised = self.measured.take_raised();
        let Some(predictor) = self.measured.predictor() else {
            self.measured.keep_raised(raised);
            return;
        };
        let now = time::now_ms();
        let taken = self.taken();
        let running = self.running();
        let cpu_budget = self.cpu_budget();
        // The first runner of each known job, in slot order, goes by its own runs; its copies have it beside them,
        // as does every runner of a repository with a busy runner whose job is not known yet.
        let mut seen = BTreeSet::new();
        let mut changed = Vec::new();
        for (profile, slots) in &mut self.slots {
            for occupant in slots.iter_mut().filter_map(Slot::occupant_mut) {
                let key = occupant.key().cloned();
                let first = key.as_ref().is_some_and(|key| seen.insert(key.clone()));
                let Some(admission) = &mut occupant.admission else {
                    continue;
                };
                // Its job's average CPU once known; until then what it started with, the largest of its queue's.
                let cpu = admission.cpu.zip(key.as_ref()).map(|(cpu, key)| {
                    job_cpu(predictor, key, profile, occupant.cpus, cpu_budget, cpu)
                });
                let mut dirty = cpu.is_some() && cpu != admission.cpu;
                admission.cpu = cpu.or(admission.cpu);
                let prediction = if let Some(key) = &key {
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
                        .listed(&occupant.repository)
                        .map(|state| state.listing().jobs())
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
                    if key.is_none() && !raised.contains(&occupant.runner_name) {
                        raised.push(occupant.runner_name.clone());
                    }
                }
                if dirty {
                    changed.push((occupant.runner_name.clone(), *admission));
                }
            }
        }
        raised.retain(|name| {
            self.occupied()
                .any(|(_, occupant)| !occupant.has_key() && occupant.runner_name == *name)
        });
        self.measured.keep_raised(raised);
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
                .filter_map(|(_, occupant)| occupant.key().cloned())
                .collect(),
            unknown: self
                .occupied()
                .filter(|(_, occupant)| occupant.runs_unknown_job())
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
        // Counted again after each stop: under the growth count a stop frees what it changes of the room kept.
        let mut tally = self.tally(self.host_remembered());
        let mut in_use = tally.count().pooled();
        let mut stops = Vec::new();
        for (slot, occupant) in self.occupied() {
            if in_use <= max {
                break;
            }
            let stopping = decisions.iter().any(
                |decision| matches!(decision, Decision::Stop { slot: other, .. } if *other == slot),
            );
            if occupant.engaged() || stopping || !self.measured.is_raised(&occupant.runner_name) {
                continue;
            }
            info!(
                "{}: stopping {}: raised to {} for a queued job, it does not fit the budget",
                occupant.repository,
                occupant.runner_name,
                crate::config::ByteSize(occupant.reserved())
            );
            tally.stop(&slot);
            in_use = tally.count().pooled();
            stops.push(Decision::Stop {
                slot,
                reason: StopReason::OverBudget,
            });
        }
        stops
    }

    /// Whether a failure holds `repository`'s pool in `profile` back at `now`.
    pub(super) fn backing_off(&self, repository: &str, profile: &str, now: SystemTime) -> bool {
        self.repositories
            .listed(repository)
            .and_then(|state| state.health(profile))
            .is_some_and(|health| health.holding(now))
    }

    /// Commits what the last plan of a tick or a pass found (ADR 0096): the pools it `held`, each wait starting with
    /// the job the plan names, and its `barrier`, which its pool remembers owning. A pool that may not start a runner,
    /// paused, backing off or preparing, loses its wait and its served mark (ADR 0098). So does a job's wait that a
    /// listing that succeeded no longer shows or a runner of this machine is known to have taken, and the mark goes once
    /// such a listing leaves the pool nothing queued. A listing that failed says nothing of the job or the queue: both
    /// stay, and the plan, which has no such job, puts up no barrier for it. A status's plan never comes here, so reading
    /// the status changes no wait.
    pub(super) fn note_waits(&mut self, held: &[Demand], barrier: Option<Barrier>) {
        let budget = self.cpu_budget();
        let in_use = self.cpu_in_use();
        let taken = self.taken();
        let now = SystemTime::now();
        let pools: Vec<(Pool, BTreeSet<u64>, bool)> = self
            .config
            .repositories
            .iter()
            .map(|repository| {
                // Not `listed_demand`: a runner no capture named yet hides the oldest job from the plan, which says
                // nothing of whether it took that one.
                let queued = self
                    .listed_unnamed(&repository.name, &taken)
                    .iter()
                    .filter(|job| matches(job, &repository.labels))
                    .map(|job| job.id)
                    .collect();
                let backing_off = self.backing_off(&repository.name, &repository.profile, now);
                let preparing = self
                    .config
                    .profiles
                    .get(&repository.profile)
                    .is_some_and(|profile| self.preparing(&repository.profile, profile).is_some());
                let stopped = self.control.is_paused(&repository.name) || backing_off || preparing;
                (Pool::from(repository), queued, stopped)
            })
            .collect();
        for (pool, queued, stopped) in pools {
            let Some(state) = self.repositories.listed_mut(&pool.repository) else {
                continue;
            };
            let listed = state.listing().is_listed();
            let waits = state.waits_entry(&pool.profile);
            if stopped {
                waits.stopped();
            } else if listed {
                waits.retain(|job| queued.contains(&job));
                if queued.is_empty() {
                    waits.emptied();
                }
            }
            let mut cpu_held = false;
            for demand in held.iter().filter(|demand| demand.pool == pool) {
                cpu_held |= demand.scarce == Scarce::Cpu;
                if !waits.note(demand.scarce, demand.job, now) {
                    continue;
                }
                match (demand.scarce, budget) {
                    (Scarce::Memory, _) => info!(
                        "{}: its next job in {} waits for memory",
                        pool.repository, pool.profile
                    ),
                    (Scarce::Cpu, Some(budget)) => info!(
                        "{}: its next job in {} waits for CPU: runners reserve {} of the budget's {}",
                        pool.repository,
                        pool.profile,
                        api::cpus(in_use),
                        api::cpus(budget)
                    ),
                    (Scarce::Cpu, None) => {}
                }
            }
            if let Some(barrier) = barrier {
                waits.own(barrier.job);
            }
            waits.set_cpu_held(cpu_held);
            if waits.is_empty() {
                state.end_waits(&pool.profile);
            }
        }
        let job = barrier.map(|barrier| barrier.job);
        if self.measured.set_barrier(job) {
            if let Some(job) = job {
                info!("job {job} waited long enough: no younger job starts until its runner fits");
            } else {
                info!("no job holds younger ones back any more");
            }
        }
    }

    /// Logs when the slice reaches the high-water mark and when it falls under it.
    pub(super) fn note_high_water(&mut self, admitting: &Admitting) {
        if !self.measured.set_high_water(admitting.held) {
            return;
        }
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
