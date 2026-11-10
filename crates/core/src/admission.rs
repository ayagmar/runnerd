//! Admission by measured peaks: a job's memory peak and average CPU predicted from the job history (`Predictor`),
//! and the history replayed through admission by caps and by predictions, with or without a CPU budget
//! (`simulate`), to show what each would have done with the same jobs, with the barrier `scheduler::plan` puts up for
//! a starved job or without, and with jobs arriving when they did or after what they followed, or as they really ran
//! (`trace`); what the host's memory leaves runners (`host`); and what all that runs counts for in the budget (`pooled`,
//! ADR 0099), with the growth count's brake (`growth`). A job's memory and usual run time go by its last runs (`recent`,
//! ADR 0070, 0097). Pure: no I/O, no clock.

pub mod growth;
pub mod host;
pub mod pooled;
pub mod recent;
mod replay;
pub mod trace;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::api::Spread;
use crate::api::{Basis, JobPeak};
use crate::config;
use crate::scheduler::Pool;
use crate::sizing::{GRANULE, History, Ran, spread};
use recent::{Ending, RECENT_RUNS, Recent};
pub use replay::{recorded, simulate};
use trace::{Hazard, OOMD_UNDER, Reading, RunId, RunTrace, STEP, THROTTLES_UNDER, Trace};

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
        predictor.read_until(history, u64::MAX, cap_of);
        predictor
    }

    /// Learns the rows of `history` that ended before `until` (ms), in the order they finished, as `read` does.
    fn read_until(&mut self, history: &History, until: u64, cap_of: impl Fn(&str) -> Option<u64>) {
        for (run, ending) in recent::rows(history) {
            let cap = run
                .memory_cap
                .or_else(|| cap_of(&run.profile))
                .unwrap_or(u64::MAX);
            let at = recent::finished(run);
            if at >= until {
                break;
            }
            match ending {
                Ending::Finished => self.learn(run, &run.profile, cap, at),
                Ending::Cancelled => self.mark(run, &run.profile, cap, at),
            }
        }
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

    /// How long a run of `key` in `profile` usually keeps its runner, if its runs say (`recent::JobRuns::usual`).
    pub fn usual(&self, key: &Key, profile: &str) -> Option<Duration> {
        self.recent.job(key, profile)?.usual()
    }

    /// The longest `usual` of `keys`, the queued jobs of a pool in `profile`, since GitHub may give its runner any of
    /// them; `None` if any has none, or there is none.
    pub fn longest_usual(
        &self,
        keys: impl IntoIterator<Item = Key>,
        profile: &str,
    ) -> Option<Duration> {
        let usual: Option<Vec<Duration>> = keys
            .into_iter()
            .map(|key| self.usual(&key, profile))
            .collect();
        usual?.into_iter().max()
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

/// Whether a replay puts up the barrier `scheduler::plan` does for a starved job (ADR 0096).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barriers {
    /// As the daemon runs: a barrier, and the younger jobs `priority::Backfill` lets start behind it (ADR 0097).
    On,
    /// A barrier nothing younger starts behind: what to compare with.
    Strict,
    /// A job that does not fit lets every later one past, however long it waited: what to compare with.
    Off,
}

/// When a replay's jobs arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrivals {
    /// When they were queued, whatever the replay did to the jobs before them.
    AsQueued,
    /// A job that queued after another of its run attempt ended follows it: it arrives as long after the replayed end
    /// of the last such job as it did after the real one. The history does not keep a job's `needs`, so this reads them
    /// from the times. A job that follows one never started does not arrive.
    AfterNeeds,
}

/// How a replay goes.
#[derive(Debug, Clone, Copy)]
pub struct Replaying<'known> {
    /// The barrier for a starved job.
    pub barriers: Barriers,
    /// When jobs arrive.
    pub arrivals: Arrivals,
    /// The history to know from the start, as the daemon does by the time these jobs queue: its rows that ended before
    /// the first job arrives. `None`, the replay knows only the runs it finishes.
    pub known: Option<&'known History>,
    /// How the jobs really ran. With it each job uses what its samples say from its replayed start, is known only when
    /// the daemon could name it, and the host's memory bounds what starts. `None`, each holds its peak for its whole run.
    pub trace: Option<&'known Trace>,
    /// The most the pooled count may take off the plain one (`pooled::Snapshot::most`); read only with a `trace`.
    pub most_credit: u64,
    /// What the host's limit reads of what runs.
    pub host_reserve: HostReserve,
    /// Whether `Backfill` expects what the credit may still give, as the daemon does.
    pub credit_room: CreditRoom,
    /// What holds the host's larger margin up.
    pub band: Band,
    /// With a `trace` and measured admission, count by the growth count instead of `most_credit` and `host_reserve`.
    pub growth: Option<GrowthCount>,
}

/// How a replay counts by the growth count (`pooled::Snapshot::largest`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrowthCount {
    /// How many jobs' growth it keeps room for.
    pub jobs: usize,
    /// Whether `growth::Growth` turns the count plain once what runs grows past the room it kept.
    pub brake: Brake,
}

/// Whether a replay's growth count has its brake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brake {
    /// As the density study proposes.
    On,
    /// To show what the brake costs and what it guards against.
    Off,
}

/// What the host's limit reads of what runs in a replay (ADR 0108).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostReserve {
    /// Every runner's take: the plain count, as before 0108 and with `[host] pooled_host = false`.
    Plain,
    /// As the daemon does: eligible runners at their use, with room kept for the two largest growths
    /// (`pooled::Count::hosted`).
    Top2,
}

/// What holds up the host's larger margin in a replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// As the daemon does: jobs wait for the host and none fits the plain margin (`host::short`).
    NoWaiterFits,
    /// To show what that gives (ADR 0102): any job that waits for the host.
    AnyWaiter,
}

/// Whether a replay's `Backfill` reads `Budget::credit_room`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditRoom {
    /// As the daemon does.
    Counted,
    /// To show what that keeps: an owner may then start before the time its younger jobs were held to.
    LeftOut,
}

impl From<Barriers> for Replaying<'_> {
    fn from(barriers: Barriers) -> Self {
        Replaying {
            barriers,
            arrivals: Arrivals::AsQueued,
            known: None,
            trace: None,
            most_credit: 0,
            host_reserve: HostReserve::Top2,
            credit_room: CreditRoom::Counted,
            band: Band::NoWaiterFits,
            growth: None,
        }
    }
}

/// What a replay did with one pool's jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolReplay {
    /// Seconds from queued to started, of each job that started.
    pub waits: Vec<u64>,
    /// The jobs the replay never started.
    pub never_started: u64,
}

impl PoolReplay {
    /// The spread of `waits`.
    pub fn wait(&self) -> Spread {
        spread(self.waits.clone())
    }
}

/// What a replay did with one attempt of a workflow run: the closest fact it has to a pull request's checks finishing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReplay {
    /// Seconds from its first job's arrival to its last job's end.
    pub took: u64,
    /// The profiles its jobs ran in.
    pub profiles: BTreeSet<String>,
}

/// A run attempt as a replay tells them apart.
type RunKey = (String, u64, u32);

/// The jobs a replay started behind a barrier (ADR 0097).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backfilled {
    /// How many.
    pub jobs: u64,
    /// Of those, how many ran past the usual run time they passed by.
    pub overran: u64,
    /// The furthest one did, in seconds.
    pub most_overrun: u64,
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
    /// By pool, the jobs that ran in it.
    pub pools: BTreeMap<Pool, PoolReplay>,
    /// What the running jobs took of the budget over the replay, in byte-milliseconds.
    pub charged: u128,
    /// The budget over the replay, in byte-milliseconds; 0 without one. `charged` over it is the busy share.
    pub capacity: u128,
    /// How long a barrier stood, in milliseconds.
    pub barrier_for: u64,
    /// What the budget had free while a barrier stood, in byte-milliseconds: at most what the barrier cost, as some of
    /// it no younger job could have used.
    pub withheld: u128,
    /// By run attempt, what the replay did with those whose jobs started.
    pub runs: Vec<RunReplay>,
    /// What started behind a barrier.
    pub backfilled: Backfilled,
    /// How long jobs waited, added up, for the slice's budget, in ms; only with a trace.
    pub slice_waited: u64,
    /// How long jobs the slice's budget had room for waited for the host's memory, added up, in ms; only with a trace.
    pub host_waited: u64,
    /// What the replay found of a pooled count's risks; only with a trace.
    pub hazard: Hazard,
}

/// What the jobs a replay runs did as they really ran, to read its figures against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recorded {
    /// By pool, the seconds each job waited from queued to its runner's start, less what passed while no runnerd ran;
    /// a job a warm runner took waited none.
    pub pools: BTreeMap<Pool, PoolReplay>,
    /// By run attempt, from its first job's queueing to its last job's end.
    pub runs: Vec<RunReplay>,
    /// How long the jobs waited for the host's memory alone, added up, in ms.
    pub host_waited: u64,
}

impl Recorded {
    /// Every job's wait added up, in seconds.
    pub fn waited(&self) -> u64 {
        self.pools.values().flat_map(|pool| &pool.waits).sum()
    }
}

#[cfg(test)]
mod tests;
