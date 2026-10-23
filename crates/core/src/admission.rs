//! Admission by measured peaks: a job's memory peak predicted from the job history (`Predictor`), and the history
//! replayed through admission by caps and by predictions (`simulate`), to show what each would have done with the
//! same jobs. Pure: no I/O, no clock.

use std::collections::BTreeMap;

use crate::api::Spread;
use crate::config::{self, Config};
use crate::sizing::{GRANULE, Ran, spread};

/// A run that peaked within this percent of its cap may have been held back or killed there.
const AT_CAP: u64 = 95;

/// For this long after a run at its cap, in milliseconds, its job is admitted at its cap: 7 days.
pub const UNPREDICTABLE_FOR: u64 = 7 * 24 * 3600 * 1000;

/// While the slice uses this percent of its `MemoryMax` or more, measured admission starts nothing.
pub const HIGH_WATER: u64 = 90;

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

/// What a prediction goes by, in the order it falls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The job's own runs, at least `min_jobs` of them: their p95 peak plus the margin.
    Job {
        /// How many.
        runs: u64,
    },
    /// Too few runs of the job but enough of its profile: the larger of the profile's p95 peak and the job's own
    /// largest, each plus the margin.
    Profile {
        /// How many runs of the profile.
        runs: u64,
    },
    /// Too few runs of the job and of its profile: the cap.
    Cap,
    /// A run of the job peaked at its cap lately: the cap, until then (ms).
    Unpredictable {
        /// When the job may be predicted again.
        until: u64,
    },
}

/// The memory admission reserves for a job, at most its cap, and what it goes by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prediction {
    /// Bytes.
    pub memory: u64,
    /// Why that much.
    pub basis: Basis,
}

/// The peaks of a set of runs, sorted, and until when a run at its cap makes them unpredictable.
#[derive(Debug, Clone, Default)]
struct Peaks {
    sorted: Vec<u64>,
    unpredictable_until: u64,
}

impl Peaks {
    fn add(&mut self, peak: u64) {
        let at = self.sorted.partition_point(|known| *known <= peak);
        self.sorted.insert(at, peak);
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

/// Predicts a job's peak from the runs it learned.
#[derive(Debug, Clone)]
pub struct Predictor {
    margin: u32,
    min_jobs: u32,
    jobs: BTreeMap<Key, Peaks>,
    profiles: BTreeMap<String, Peaks>,
}

impl Predictor {
    /// A predictor that knows no run yet, with `[sizing]`'s margin and minimum runs.
    pub fn new(rules: &config::Sizing) -> Predictor {
        Predictor {
            margin: rules.margin,
            min_jobs: rules.min_jobs,
            jobs: BTreeMap::new(),
            profiles: BTreeMap::new(),
        }
    }

    /// Learns a run of `profile` that finished at `at` (ms), having run under `cap`.
    pub fn learn(&mut self, run: &Ran, profile: &str, cap: u64, at: u64) {
        let peak = run.memory_peak;
        let job = self.jobs.entry(Key::of(run)).or_default();
        job.add(peak);
        if peak.saturating_mul(100) >= cap.saturating_mul(AT_CAP) {
            job.unpredictable_until = job.unpredictable_until.max(at + UNPREDICTABLE_FOR);
        }
        self.profiles
            .entry(profile.to_owned())
            .or_default()
            .add(peak);
    }

    /// The memory to reserve for a run of `key` in `profile`, whose cap is `cap`, at `now` (ms).
    pub fn predict(&self, key: &Key, profile: &str, cap: u64, now: u64) -> Prediction {
        let job = self.jobs.get(key);
        if let Some(until) = job
            .map(|job| job.unpredictable_until)
            .filter(|until| *until > now)
        {
            return Prediction {
                memory: cap,
                basis: Basis::Unpredictable { until },
            };
        }
        let min_jobs = u64::from(self.min_jobs);
        let (memory, basis) = match (job, self.profiles.get(profile)) {
            (Some(job), _) if job.runs() >= min_jobs => {
                (self.with_margin(job.p95()), Basis::Job { runs: job.runs() })
            }
            (job, Some(peers)) if peers.runs() >= min_jobs => {
                let own = job.map_or(0, |job| self.with_margin(job.max()));
                (
                    own.max(self.with_margin(peers.p95())),
                    Basis::Profile { runs: peers.runs() },
                )
            }
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
    /// When a slot is free, the predictions of the running jobs and its own fit the budget, and the running jobs'
    /// peaks are under `HIGH_WATER` percent of it.
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
    /// running job peaked at once, which is more than the slice ever holds.
    pub worst_overlap: u64,
    /// Seconds from the first job queued to the last one finished.
    pub makespan: u64,
    /// Jobs whose peak was above what admission reserved for them.
    pub under_predicted: u64,
    /// How many times the high-water mark held a job back that the predictions had room for.
    pub held_back: u64,
}

/// A job the replay started.
struct Running<'run> {
    run: &'run Ran,
    profile: &'run str,
    ends: u64,
    reserved: u64,
    peak: u64,
}

/// Replays `jobs` through `policy`: each arrives when it was queued and runs as long as it ran, in a profile with
/// `slots` (by name; the config's where absent) under the slice's `memory_max`. Jobs start as soon as the policy
/// admits them, oldest first, a job that does not fit letting a later one past, as `scheduler::plan` does.
/// Measured admission predicts from the runs finished before it decides, as the daemon would.
pub fn simulate(
    config: &Config,
    memory_max: Option<u64>,
    jobs: &[Ran],
    policy: Policy,
    slots: &BTreeMap<String, u32>,
) -> Replay {
    let mirror = if config.caches.mirror.enabled {
        config.caches.mirror.memory.0
    } else {
        0
    };
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
        while let Some(run) = queue.get(next).filter(|run| run.queued_at <= now) {
            waiting.push(run);
            next += 1;
        }
        waiting.retain(|run| !replayer.admit(run, now));
        replayer.most_running = replayer.most_running.max(replayer.running.len() as u64);
        let peaks: u64 = replayer.running.iter().map(|running| running.peak).sum();
        replayer.worst_overlap = replayer.worst_overlap.max(peaks + mirror);
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
    }
}

/// A replay's state between two moments.
struct Replayer<'run> {
    config: &'run Config,
    budget: Option<u64>,
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
}

impl<'run> Replayer<'run> {
    /// Ends the jobs that end by `now`; measured admission learns their peaks.
    fn finish(&mut self, now: u64) {
        let (ended, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.running)
            .into_iter()
            .partition(|running| running.ends <= now);
        self.running = running;
        for ended in ended {
            let cap = self.config.profiles[ended.profile].memory.0;
            self.predictor
                .learn(ended.run, ended.profile, cap, ended.ends);
            self.last_end = self.last_end.max(ended.ends);
        }
    }

    /// Starts `run` at `now` if the policy admits it; whether it did.
    fn admit(&mut self, run: &'run Ran, now: u64) -> bool {
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
        let reserved = match self.policy {
            Policy::Caps => cap,
            Policy::Measured => {
                self.predictor
                    .predict(&Key::of(run), profile, cap, now)
                    .memory
            }
        };
        if let Some(budget) = self.budget {
            let taken: u64 = self.running.iter().map(|running| running.reserved).sum();
            if taken + reserved > budget {
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
        // A job over its cap is killed at it.
        let peak = run.memory_peak.min(cap);
        self.under_predicted += u64::from(peak > reserved);
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
        });
        true
    }
}

#[cfg(test)]
mod tests;
