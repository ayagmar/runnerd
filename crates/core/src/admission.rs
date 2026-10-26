//! Admission by measured peaks: a job's memory peak and average CPU predicted from the job history (`Predictor`),
//! and the history replayed through admission by caps and by predictions, with or without a CPU budget
//! (`simulate`), to show what each would have done with the same jobs; and what the host's memory leaves runners
//! (`host`). A job's memory goes by its last runs (`recent`, ADR 0070). Pure: no I/O, no clock.

pub mod host;
pub mod recent;

use std::collections::{BTreeMap, BTreeSet};

use crate::api::Spread;
use crate::api::{Basis, JobPeak};
use crate::config::{self, Config};
use crate::sizing::{GRANULE, History, Ran, spread};
use recent::{Ending, RECENT_RUNS, Recent};

/// For at most this long after a run at its cap or killed for memory, in milliseconds, its job is admitted at its
/// cap: 7 days, or until `recent::CLEAN_RUNS` clear runs. After a run past what it reserved, its job reserves its
/// largest peak for at most as long, or until the run leaves its window.
pub const UNPREDICTABLE_FOR: u64 = 7 * 24 * 3600 * 1000;

/// From this many successful runs of its own, fewer than `[sizing] min_jobs` runs, a job reserves its own largest
/// peak while no other copy of it is queued or running; otherwise the larger of that and its profile's p95.
pub const OWN_RUNS: u64 = 2;

/// While the slice uses this percent of its `MemoryMax` or more, measured admission starts nothing.
pub const HIGH_WATER: u64 = 90;

/// CPU predictions round up to this many millicores: a tenth of a CPU.
pub const CPU_GRANULE: u64 = 100;

/// A job as admission tells jobs apart: its repository, workflow and name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name, when known.
    pub workflow: Option<String>,
    /// The job's name in the workflow.
    pub name: String,
}

impl Key {
    /// A finished run's key.
    pub fn of(run: &Ran) -> Key {
        Key {
            repository: run.repository.clone(),
            workflow: run.workflow.clone(),
            name: run.name.clone(),
        }
    }
}

/// Whether other copies of a job may run beside the one predicted: queued with it, or running. A few light runs
/// say little about a burst of copies, after a cache was invalidated, so only one copy at a time goes by them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copies {
    /// No other copy is queued or running.
    Alone,
    /// Another copy is queued or running.
    Several,
}

/// The memory admission reserves for a job, at most its cap, and what it goes by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prediction {
    /// Bytes.
    pub memory: u64,
    /// Why that much.
    pub basis: Basis,
}

/// The CPU a runner reserves for a job, in millicores, at most its profile's `cpus`, and what it goes by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuPrediction {
    /// Millicores.
    pub cpu: u64,
    /// Why that much: `Basis::Job`, `Profile`, `ProfileMax` or `Cap`.
    pub basis: Basis,
}

/// The CPU averages of a set of runs, sorted, and how many of those runs succeeded.
#[derive(Debug, Clone, Default)]
struct Averages {
    sorted: Vec<u64>,
    succeeded: u64,
}

impl Averages {
    fn add(&mut self, average: u64, succeeded: bool) {
        let at = self.sorted.partition_point(|known| *known <= average);
        self.sorted.insert(at, average);
        self.succeeded += u64::from(succeeded);
    }

    fn runs(&self) -> u64 {
        self.sorted.len() as u64
    }

    fn p95(&self) -> u64 {
        spread(self.sorted.clone()).p95
    }

    fn max(&self) -> u64 {
        self.sorted.last().copied().unwrap_or(0)
    }
}

/// A job in one profile, as the predictor keeps its CPU averages.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct InProfile {
    key: Key,
    profile: String,
}

impl InProfile {
    fn new(key: &Key, profile: &str) -> InProfile {
        InProfile {
            key: key.clone(),
            profile: profile.to_owned(),
        }
    }
}

/// Predicts a job's peak from the runs it learned.
#[derive(Debug, Clone)]
pub struct Predictor {
    margin: u32,
    min_jobs: u32,
    /// Each job's last runs, by job and the profile it ran in (ADR 0070).
    recent: Recent,
    /// The CPU averages of the runs that have one, apart from the peaks: a run whose peak says nothing of its
    /// memory still says what CPU it used, and must not make its job known to memory predictions.
    cpu_jobs: BTreeMap<InProfile, Averages>,
    cpu_profiles: BTreeMap<String, Averages>,
}

impl Predictor {
    /// A predictor that knows no run yet, with `[sizing]`'s margin and minimum runs.
    pub fn new(rules: &config::Sizing) -> Predictor {
        Predictor {
            margin: rules.margin,
            min_jobs: rules.min_jobs,
            recent: Recent::new(RECENT_RUNS.max(u64::from(rules.min_jobs))),
            cpu_jobs: BTreeMap::new(),
            cpu_profiles: BTreeMap::new(),
        }
    }

    /// A predictor that learned every row of `history`, in the order they finished, each under the cap its
    /// container had, else `cap_of(profile)`: what the daemon reads every minute, and again after a restart.
    pub fn read(
        rules: &config::Sizing,
        history: &History,
        cap_of: impl Fn(&str) -> Option<u64>,
    ) -> Predictor {
        let mut predictor = Predictor::new(rules);
        for (run, ending) in recent::rows(history) {
            let cap = run
                .memory_cap
                .or_else(|| cap_of(&run.profile))
                .unwrap_or(u64::MAX);
            let at = recent::finished(run);
            match ending {
                Ending::Finished => predictor.learn(run, &run.profile, cap, at),
                Ending::Cancelled => predictor.mark(run, &run.profile, cap, at),
            }
        }
        predictor
    }

    /// Learns a finished run of `profile` that ended at `at` (ms), having run under `cap`: its peak, how it ended,
    /// and its CPU. Runs must come in the order they finished (`recent::Recent::read`).
    pub fn learn(&mut self, run: &Ran, profile: &str, cap: u64, at: u64) {
        self.recent.read(run, profile, Ending::Finished, cap, at);
        self.learn_cpu(run, profile);
    }

    /// Learns the CPU a finished run of `profile` used, without its peak.
    pub fn learn_cpu(&mut self, run: &Ran, profile: &str) {
        let Some(average) = run.cpu_average else {
            return;
        };
        self.cpu_profiles
            .entry(profile.to_owned())
            .or_default()
            .add(average, run.succeeded);
        self.cpu_jobs
            .entry(InProfile::new(&Key::of(run), profile))
            .or_default()
            .add(average, run.succeeded);
    }

    /// The CPU to reserve for a run of `key` in `profile`, whose `cpus` are `cap` millicores: the job's average
    /// CPU (its CPU time over its run time), by the same fallbacks as memory without the margin. CPU is shared, not
    /// a kill limit, so neither a run at its cap nor copies of the job beside it change it.
    pub fn predict_cpu(&self, key: &Key, profile: &str, cap: u64) -> CpuPrediction {
        let job = self.cpu_jobs.get(&InProfile::new(key, profile));
        let min_jobs = u64::from(self.min_jobs);
        let peers = self
            .cpu_profiles
            .get(profile)
            .filter(|peers| peers.runs() >= min_jobs);
        let (cpu, basis) = match (job.filter(|job| job.runs() > 0), peers) {
            (Some(job), _) if job.runs() >= min_jobs => (
                job.p95(),
                Basis::Job {
                    runs: job.runs(),
                    peak: JobPeak::P95,
                },
            ),
            (Some(job), Some(_)) if job.succeeded >= OWN_RUNS => (
                job.max(),
                Basis::Job {
                    runs: job.runs(),
                    peak: JobPeak::Largest,
                },
            ),
            (Some(job), Some(peers)) => (
                job.max().max(peers.p95()),
                Basis::Profile { runs: peers.runs() },
            ),
            (None, Some(peers)) => (peers.max(), Basis::ProfileMax { runs: peers.runs() }),
            _ => (cap, Basis::Cap),
        };
        CpuPrediction {
            cpu: cpu
                .div_ceil(CPU_GRANULE)
                .saturating_mul(CPU_GRANULE)
                .clamp(CPU_GRANULE, cap.max(CPU_GRANULE)),
            basis,
        }
    }

    /// Learns a cancelled run of `profile`: how it ended, and its peak only when it met its cap. A run that met
    /// its cap or was killed for memory holds its job in that profile at its cap (`Ran::capped`), a peak past what
    /// it reserved makes it reserve its largest peak while that run is in its window.
    pub fn mark(&mut self, run: &Ran, profile: &str, cap: u64, at: u64) {
        self.recent.read(run, profile, Ending::Cancelled, cap, at);
    }

    /// The memory to reserve for a run of `key` in `profile`, whose cap is `cap`, at `now` (ms), with `copies`
    /// of it beside.
    pub fn predict(
        &self,
        key: &Key,
        profile: &str,
        cap: u64,
        now: u64,
        copies: Copies,
    ) -> Prediction {
        let job = self.recent.job(key, profile);
        if let Some(hold) = job.and_then(|job| job.held(now, Some(UNPREDICTABLE_FOR))) {
            return Prediction {
                memory: cap,
                basis: Basis::Unpredictable {
                    until: hold.at.saturating_add(UNPREDICTABLE_FOR) / 1000,
                },
            };
        }
        let min_jobs = u64::from(self.min_jobs);
        let peers = self.recent.profile(profile);
        let peers_runs = peers.len() as u64;
        let overran = job.and_then(|job| {
            job.overruns(now, UNPREDICTABLE_FOR)
                .fold(None, |most: Option<(u64, u64)>, overrun| {
                    let until = overrun.at.saturating_add(UNPREDICTABLE_FOR);
                    Some(most.map_or((overrun.peak, until), |(peak, last)| {
                        (peak.max(overrun.peak), last.max(until))
                    }))
                })
        });
        let (memory, basis) = match (job, peers_runs >= min_jobs) {
            // Seen only cancelled, so its peaks say nothing.
            (Some(job), _) if job.runs() == 0 => (cap, Basis::Cap),
            (Some(job), _) if let Some((peak, until)) = overran => (
                self.with_margin(job.max().max(peak)),
                Basis::Overran {
                    until: until / 1000,
                },
            ),
            (Some(job), _) if job.runs() >= min_jobs => (
                self.with_margin(spread(job.peaks().to_vec()).p95),
                Basis::Job {
                    runs: job.runs(),
                    peak: JobPeak::P95,
                },
            ),
            (Some(job), true) if job.succeeded() >= OWN_RUNS && copies == Copies::Alone => (
                self.with_margin(job.max()),
                Basis::Job {
                    runs: job.runs(),
                    peak: JobPeak::Largest,
                },
            ),
            (Some(job), true) => (
                self.with_margin(job.max().max(spread(peers.to_vec()).p95)),
                Basis::Profile { runs: peers_runs },
            ),
            // A job never seen may be the profile's heaviest.
            (None, true) => (
                self.with_margin(peers.last().copied().unwrap_or(0)),
                Basis::ProfileMax { runs: peers_runs },
            ),
            _ => (cap, Basis::Cap),
        };
        Prediction {
            memory: memory.min(cap),
            basis,
        }
    }
    /// `peak` plus the margin, rounded up to 256M.
    fn with_margin(&self, peak: u64) -> u64 {
        let memory = peak.saturating_add(peak.saturating_mul(u64::from(self.margin)) / 100);
        memory
            .div_ceil(GRANULE)
            .saturating_mul(GRANULE)
            .max(GRANULE)
    }
}

/// How a replay admits a queued job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// When a slot is free and the caps of the running jobs and its own fit the budget: what runnerd does now.
    Caps,
    /// As the daemon admits: when a slot is free, the running jobs (each at the larger of its prediction and its
    /// peak) and the largest prediction among the repository's waiting jobs fit the budget, and the running jobs'
    /// peaks are under `HIGH_WATER` percent of it. A started job then reserves its own prediction.
    Measured,
}

/// What a replay did with the history's jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// The jobs replayed: finished ones of a configured profile.
    pub jobs: u64,
    /// Of those, the ones the policy never started: more than the budget holds.
    pub never_started: u64,
    /// Seconds from queued to started.
    pub wait: Spread,
    /// The mean of those, in seconds.
    pub mean_wait: u64,
    /// The most jobs that ran at once.
    pub most_running: u64,
    /// The most memory, in bytes, the running jobs' peaks and the mirror add up to at one moment: as if every
    /// running job peaked at once, which is more than the slice ever holds. Not a bound on the daemon, which can
    /// overlap more: it sees what jobs use, less than their peaks while they warm up, and the replay leaves out
    /// start latency, seeds, shells and cancelled jobs.
    pub worst_overlap: u64,
    /// Seconds from the first job queued to the last one finished.
    pub makespan: u64,
    /// Jobs whose peak was above what admission reserved for them when they ended.
    pub under_predicted: u64,
    /// How many times the high-water mark held a job back that the predictions had room for.
    pub held_back: u64,
    /// How many times the CPU budget held a job back that memory and its slots had room for.
    pub cpu_held: u64,
    /// The most CPU, in millicores, the running jobs' averages add up to at one moment.
    pub worst_cpu: u64,
}

/// A job the replay started.
struct Running<'run> {
    run: &'run Ran,
    profile: &'run str,
    ends: u64,
    reserved: u64,
    peak: u64,
    /// The CPU it reserves, in millicores.
    cpu: u64,
}

/// Replays `jobs` through `policy`: each arrives when it was queued and runs as long as it ran, in a profile with
/// `slots` (by name; the config's where absent) under the slice's `memory_max`. Jobs start as soon as the policy
/// admits them, oldest first, a job that does not fit letting a later one past, as `scheduler::plan` does.
/// Measured admission predicts from the runs finished before it decides, as the daemon would, and holds each
/// running job at its peak for its whole run, which the daemon does not see. A job with copies waiting or running
/// beside it is predicted as one with `Copies::Several`. Each moment, the running jobs are predicted again, the
/// first running copy of a job going by its own runs, as the daemon does each tick. Unlike the daemon, the replay
/// knows a job from its start: it never sees a runner whose job is not known yet, and the order jobs started
/// stands for the daemon's slot order.
///
/// With a `cpu_budget` in millicores, a job also starts only when the CPU the running jobs reserve and its own fit
/// it: by caps, each its profile's `cpus`; measured, each its predicted average (`Predictor::predict_cpu`), at most
/// the budget. The replay holds each job's run time whatever the CPU it shares: it shows what admission holds back,
/// not how much a busy CPU slows the jobs.
pub fn simulate(
    config: &Config,
    memory_max: Option<u64>,
    cpu_budget: Option<u64>,
    jobs: &[Ran],
    policy: Policy,
    slots: &BTreeMap<String, u32>,
) -> Replay {
    let mirror = config.mirror_memory();
    let budget = memory_max.map(|max| max.saturating_sub(mirror));
    let mut queue: Vec<&Ran> = jobs
        .iter()
        .filter(|run| config.profiles.contains_key(&run.profile))
        .filter(|run| matches!((run.started_at, run.finished_at), (Some(start), Some(end)) if end >= start))
        .collect();
    queue.sort_by_key(|run| run.queued_at);
    let mut replayer = Replayer {
        config,
        budget,
        cpu_budget,
        policy,
        slots,
        predictor: Predictor::new(&config.sizing),
        running: Vec::new(),
        waits: Vec::new(),
        most_running: 0,
        worst_overlap: 0,
        last_end: 0,
        under_predicted: 0,
        held_back: 0,
        cpu_held: 0,
        worst_cpu: 0,
    };
    let mut waiting: Vec<&Ran> = Vec::new();
    let mut next = 0;
    loop {
        let arrival = queue.get(next).map(|run| run.queued_at);
        let end = replayer.running.iter().map(|running| running.ends).min();
        let Some(now) = [arrival, end].into_iter().flatten().min() else {
            break;
        };
        replayer.finish(now);
        replayer.rereserve(now);
        while let Some(run) = queue.get(next).filter(|run| run.queued_at <= now) {
            waiting.push(run);
            next += 1;
        }
        let mut index = 0;
        while let Some(run) = waiting.get(index).copied() {
            if replayer.admit(run, &waiting, now) {
                waiting.remove(index);
            } else {
                index += 1;
            }
        }
        replayer.most_running = replayer.most_running.max(replayer.running.len() as u64);
        let peaks: u64 = replayer.running.iter().map(|running| running.peak).sum();
        replayer.worst_overlap = replayer.worst_overlap.max(peaks + mirror);
        let cpu: u64 = replayer
            .running
            .iter()
            .map(|running| running.run.cpu_average.unwrap_or(0))
            .sum();
        replayer.worst_cpu = replayer.worst_cpu.max(cpu);
    }
    let waits = replayer.waits;
    let first = queue.first().map_or(0, |run| run.queued_at);
    Replay {
        jobs: queue.len() as u64,
        never_started: waiting.len() as u64,
        mean_wait: waits.iter().sum::<u64>() / (waits.len() as u64).max(1),
        wait: spread(waits),
        most_running: replayer.most_running,
        worst_overlap: replayer.worst_overlap,
        makespan: replayer.last_end.saturating_sub(first) / 1000,
        under_predicted: replayer.under_predicted,
        held_back: replayer.held_back,
        cpu_held: replayer.cpu_held,
        worst_cpu: replayer.worst_cpu,
    }
}

/// A replay's state between two moments.
struct Replayer<'run> {
    config: &'run Config,
    budget: Option<u64>,
    cpu_budget: Option<u64>,
    policy: Policy,
    slots: &'run BTreeMap<String, u32>,
    predictor: Predictor,
    running: Vec<Running<'run>>,
    waits: Vec<u64>,
    most_running: u64,
    worst_overlap: u64,
    last_end: u64,
    under_predicted: u64,
    held_back: u64,
    cpu_held: u64,
    worst_cpu: u64,
}

impl<'run> Replayer<'run> {
    /// Ends the jobs that end by `now`, in the order they end; measured admission learns them as the daemon reads
    /// their rows.
    fn finish(&mut self, now: u64) {
        let (ended, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.running)
            .into_iter()
            .partition(|running| running.ends <= now);
        self.running = running;
        let mut ended = ended;
        ended.sort_by_key(|ended| ended.ends);
        for ended in ended {
            // As the daemon reads the row: under the cap its container had.
            let cap = ended
                .run
                .memory_cap
                .unwrap_or(self.config.profiles[ended.profile].memory.0);
            self.under_predicted += u64::from(ended.peak > ended.reserved);
            let reserved = (self.policy == Policy::Measured).then_some(ended.reserved);
            let run = Ran {
                reserved,
                ..ended.run.clone()
            };
            self.predictor.learn(&run, ended.profile, cap, ended.ends);
            self.last_end = self.last_end.max(ended.ends);
        }
    }

    /// Under measured admission, predicts each running job again, as the daemon's runners follow their job's
    /// prediction every tick: the first running copy of a job, in the order they started, goes by its own runs, and
    /// the others have it beside them.
    fn rereserve(&mut self, now: u64) {
        if self.policy != Policy::Measured {
            return;
        }
        let cpus: Vec<u64> = self
            .running
            .iter()
            .map(|running| self.cpu(&Key::of(running.run), running.profile))
            .collect();
        let mut seen = BTreeSet::new();
        for (running, cpu) in self.running.iter_mut().zip(cpus) {
            let key = Key::of(running.run);
            let copies = if seen.insert(key.clone()) {
                Copies::Alone
            } else {
                Copies::Several
            };
            let limits = &self.config.profiles[running.profile];
            running.reserved = self
                .predictor
                .predict(&key, running.profile, limits.memory.0, now, copies)
                .memory;
            running.cpu = cpu;
        }
    }

    /// The CPU a run of `key` in `profile` reserves, in millicores: its profile's `cpus` by caps, its predicted
    /// average measured, at most the CPU budget.
    fn cpu(&self, key: &Key, profile: &str) -> u64 {
        let cap = u64::from(self.config.profiles[profile].cpus).saturating_mul(1000);
        let cpu = match self.policy {
            Policy::Caps => cap,
            Policy::Measured => self.predictor.predict_cpu(key, profile, cap).cpu,
        };
        self.cpu_budget.map_or(cpu, |budget| cpu.min(budget))
    }

    /// What a running job takes of the budget: under measured admission, as the daemon counts it, the larger of
    /// what it reserved and what it uses, here its peak for the whole run.
    fn taken(&self) -> u64 {
        self.running
            .iter()
            .map(|running| match self.policy {
                Policy::Caps => running.reserved,
                Policy::Measured => running.reserved.max(running.peak),
            })
            .sum()
    }

    /// Starts `run` at `now` if the policy admits it, `waiting` being every job not started yet; whether it did.
    /// As in the daemon, the runner starts reserving the largest prediction among the waiting jobs of its
    /// repository's pool (those that ran in its profile), then its job's own.
    fn admit(&mut self, run: &'run Ran, waiting: &[&Ran], now: u64) -> bool {
        let Some((profile, limits)) = self.config.profiles.get_key_value(&run.profile) else {
            return false;
        };
        let slots = self.slots.get(profile).copied().unwrap_or(limits.slots);
        let in_profile = self
            .running
            .iter()
            .filter(|running| running.profile == profile.as_str())
            .count();
        if in_profile >= slots as usize {
            return false;
        }
        let cap = limits.memory.0;
        let running = |key: &Key| {
            self.running
                .iter()
                .any(|running| Key::of(running.run) == *key)
        };
        // A waiting job with a copy waiting or running beside it, as the daemon sees its queue.
        let queued = |job: &Ran| {
            let key = Key::of(job);
            let waiting = waiting.iter().filter(|other| Key::of(other) == key).count();
            let copies = if waiting > 1 || running(&key) {
                Copies::Several
            } else {
                Copies::Alone
            };
            self.predictor
                .predict(&key, profile, cap, now, copies)
                .memory
        };
        // The started job, once known: the first of its copies to run goes by its own runs.
        let started = || {
            let key = Key::of(run);
            let copies = if running(&key) {
                Copies::Several
            } else {
                Copies::Alone
            };
            self.predictor
                .predict(&key, profile, cap, now, copies)
                .memory
        };
        let (starting, reserved) = match self.policy {
            Policy::Caps => (cap, cap),
            Policy::Measured => (
                waiting
                    .iter()
                    .filter(|job| job.repository == run.repository && job.profile == run.profile)
                    .map(|job| queued(job))
                    .max()
                    .unwrap_or(0)
                    .max(started()),
                started(),
            ),
        };
        if let Some(budget) = self.budget {
            if self.taken() + starting > budget {
                return false;
            }
            let peaks: u64 = self.running.iter().map(|running| running.peak).sum();
            if self.policy == Policy::Measured
                && peaks.saturating_mul(100) >= budget.saturating_mul(HIGH_WATER)
            {
                self.held_back += 1;
                return false;
            }
        }
        let cpu = self.cpu(&Key::of(run), profile);
        if let Some(cpu_budget) = self.cpu_budget {
            // As the daemon starts a runner: at the most CPU any waiting job of its repository's pool reserves.
            let starting = waiting
                .iter()
                .filter(|job| job.repository == run.repository && job.profile == run.profile)
                .map(|job| self.cpu(&Key::of(job), profile))
                .max()
                .unwrap_or(0)
                .max(cpu);
            let taken: u64 = self.running.iter().map(|running| running.cpu).sum();
            if taken + starting > cpu_budget {
                self.cpu_held += 1;
                return false;
            }
        }
        // A job over its cap is killed at it.
        let peak = run.memory_peak.min(cap);
        let ran_for = run
            .finished_at
            .unwrap_or_default()
            .saturating_sub(run.started_at.unwrap_or_default());
        self.waits.push(now.saturating_sub(run.queued_at) / 1000);
        self.running.push(Running {
            run,
            profile,
            ends: now + ran_for,
            reserved,
            peak,
            cpu,
        });
        true
    }
}

#[cfg(test)]
mod tests;
