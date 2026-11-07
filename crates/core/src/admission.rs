//! Admission by measured peaks: a job's memory peak and average CPU predicted from the job history (`Predictor`),
//! and the history replayed through admission by caps and by predictions, with or without a CPU budget
//! (`simulate`), to show what each would have done with the same jobs, with the barrier `scheduler::plan` puts up for
//! a starved job or without, and with jobs arriving when they did or after what they followed, or as they really ran
//! (`trace`); what the host's memory leaves runners (`host`); and what all that runs counts for in the budget (`pooled`,
//! ADR 0099). A job's memory and usual run time go by its last runs (`recent`, ADR 0070, 0097). Pure: no I/O, no clock.

pub mod host;
pub mod pooled;
pub mod recent;
pub mod trace;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use crate::api::Spread;
use crate::api::{Basis, HostMemory, JobPeak};
use crate::config::{self, Config};
use crate::scheduler::priority::{Around, Backfill, Candidate, Caps, Held, Older, Runner, Short};
use crate::scheduler::{Barrier, Drained, Left, Pool, Scarce, Waits};
use crate::sizing::{GRANULE, History, Measure, Ran, spread};
use crate::time;
use recent::{Ending, RECENT_RUNS, Recent};
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

/// A job the replay started.
struct Running<'run> {
    run: &'run Ran,
    /// Its place in the jobs replayed.
    job: u64,
    profile: &'run str,
    /// When it started, in ms.
    started: u64,
    ends: u64,
    /// When it usually ends by, in ms, if it started behind a barrier.
    backfilled: Option<u64>,
    reserved: u64,
    peak: u64,
    /// The CPU it reserves, in millicores.
    cpu: u64,
    /// Its job.
    key: Key,
    /// What its reservation goes by.
    basis: Basis,
    /// What it reserves until its job is known: its pool's largest prediction when it started.
    starting: u64,
    /// How it ran; `None` without a trace or a run the trace has no samples of.
    trace: Option<&'run RunTrace>,
}

/// Replays `jobs` through `policy`: each arrives as `how` says and runs as long as it ran, in a profile with
/// `slots` (by name; the config's where absent) under the slice's `memory_max`. Jobs start as soon as the policy
/// admits them, in `jobs`' order, which is the job IDs' as the history reads them and `scheduler::plan` serves them: a
/// job that does not fit lets a later one past, until it waited long enough under `how`'s barriers and no later one
/// starts before its pool does, but those `priority::Backfill` lets by. The replay looks again at each such limit, so a
/// quiet queue reaches it too. Usual run times come from `how`'s known runs and those finished before each moment, as
/// predictions do.
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
///
/// With a `Replaying::trace`, the replay runs as the daemon counts: each job uses what its own samples say, shifted to
/// its replayed start, and takes the larger of that and what it reserves; it reserves its pool's largest prediction
/// until the daemon could name its job; a stale sample counts at its cap; the budget goes by `pooled::count`; the host's
/// recorded programs bound what starts, with the margin of `host`; and the mark reads the runners' use with the trace's
/// overhead. It looks every `trace::STEP` and plans every `[host] poll_interval` and at each arrival and end.
pub fn simulate<'known>(
    config: &Config,
    memory_max: Option<u64>,
    cpu_budget: Option<u64>,
    jobs: &[Ran],
    policy: Policy,
    slots: &BTreeMap<String, u32>,
    how: impl Into<Replaying<'known>>,
) -> Replay {
    let Replaying {
        barriers,
        arrivals,
        known,
        trace,
        most_credit,
        host_reserve,
        credit_room,
        band,
    } = how.into();
    let mirror = config.mirror_memory();
    let budget = memory_max.map(|max| max.saturating_sub(mirror));
    let queue = replayable(config, jobs);
    let followers = match arrivals {
        Arrivals::AsQueued => BTreeMap::new(),
        Arrivals::AfterNeeds => followers(&queue),
    };
    let following: BTreeSet<u64> = followers
        .values()
        .flatten()
        .map(|follower| queue[follower.at].job)
        .collect();
    let arriving = (0_usize..)
        .zip(&queue)
        .filter(|(_, queued)| !following.contains(&queued.job))
        .map(|(at, queued)| (queued.run.queued_at, at))
        .collect();
    let mut predictor = Predictor::new(&config.sizing);
    if let (Some(known), Some(first)) = (known, queue.first()) {
        predictor.read_until(known, first.run.queued_at, |profile| {
            config.profiles.get(profile).map(|limits| limits.memory.0)
        });
    }
    let replayer = Replayer {
        config,
        budget,
        cpu_budget,
        policy,
        slots,
        predictor,
        running: Vec::new(),
        waits: Vec::new(),
        most_running: 0,
        worst_overlap: 0,
        last_end: 0,
        under_predicted: 0,
        held_back: 0,
        cpu_held: 0,
        worst_cpu: 0,
        barriers,
        pool_waits: BTreeMap::new(),
        pools: BTreeMap::new(),
        barrier: None,
        backfill: None,
        usual: BTreeMap::new(),
        arriving,
        followers,
        started: Vec::new(),
        runs: BTreeMap::new(),
        backfilled: Backfilled::default(),
        moment: queue.first().map_or(0, |queued| queued.run.queued_at),
        charged: 0,
        capacity: 0,
        barrier_for: 0,
        withheld: 0,
        memory_max,
        mirror,
        trace,
        most_credit,
        host_reserve,
        expects_credit: credit_room,
        short: false,
        next_plan: 0,
        needs: RefCell::new(BTreeMap::new()),
        held_for_slice: 0,
        held_for_host: 0,
        host_needs: Vec::new(),
        band,
        slice_waited: 0,
        host_waited: 0,
        hazard: Hazard::default(),
        last_credit: 0,
        last_host_credit: 0,
        cohorts: Vec::new(),
    };
    replayer.play(&queue)
}

/// `jobs` a replay can run, in the order they queued: those of a known profile with a run time.
fn replayable<'run>(config: &Config, jobs: &'run [Ran]) -> Vec<Queued<'run>> {
    let mut queue: Vec<Queued<'_>> = (0_u64..)
        .zip(jobs)
        .filter(|(_, run)| config.profiles.contains_key(&run.profile))
        .filter(|(_, run)| matches!((run.started_at, run.finished_at), (Some(start), Some(end)) if end >= start))
        .map(|(job, run)| Queued {
            job,
            run,
            arrived: run.queued_at,
        })
        .collect();
    queue.sort_by_key(|queued| queued.run.queued_at);
    queue
}

/// A job waiting in a replay.
#[derive(Clone, Copy)]
struct Queued<'run> {
    /// Its place in the jobs replayed, which stands for its ID: the order `scheduler::plan` serves jobs in.
    job: u64,
    run: &'run Ran,
    /// When it arrived in the replay, in ms.
    arrived: u64,
}

/// A job that follows another in its run attempt.
#[derive(Clone, Copy)]
struct Follower {
    /// Its place in the replay's queue.
    at: usize,
    /// How long after the job it follows it queued, in ms.
    gap: u64,
}

/// By the job each follows, the jobs of `queue` that queued after another job of their run attempt ended: a job
/// follows the last that ended before it queued, as GitHub queues a job once the last of its `needs` ends. Only a job
/// queued earlier can be followed, so nothing follows itself through a job that took no time.
fn followers(queue: &[Queued<'_>]) -> BTreeMap<u64, Vec<Follower>> {
    let mut attempts: BTreeMap<RunKey, Vec<usize>> = BTreeMap::new();
    for (at, queued) in queue.iter().enumerate() {
        attempts.entry(run_key(queued.run)).or_default().push(at);
    }
    let mut followers: BTreeMap<u64, Vec<Follower>> = BTreeMap::new();
    for attempt in attempts.values() {
        for &at in attempt {
            let queued_at = queue[at].run.queued_at;
            let followed = attempt
                .iter()
                .map(|other| &queue[*other])
                .filter(|other| other.run.queued_at < queued_at)
                .filter_map(|other| Some((other.run.finished_at?, other.job)))
                .filter(|(ended, _)| *ended <= queued_at)
                .max();
            if let Some((ended, job)) = followed {
                let gap = queued_at - ended;
                followers.entry(job).or_default().push(Follower { at, gap });
            }
        }
    }
    followers
}

/// The run attempt `run` belongs to.
fn run_key(run: &Ran) -> RunKey {
    (run.repository.clone(), run.run_id, run.run_attempt)
}

/// What a replay saw of a run attempt so far.
struct RunSpan {
    first_arrival: u64,
    last_end: u64,
    profiles: BTreeSet<String>,
}

impl RunSpan {
    fn replay(self) -> RunReplay {
        RunReplay {
            took: self.last_end.saturating_sub(self.first_arrival) / 1000,
            profiles: self.profiles,
        }
    }
}

/// The pool a job ran in.
fn pool_of(run: &Ran) -> Pool {
    Pool {
        repository: run.repository.clone(),
        profile: run.profile.clone(),
    }
}

/// What a job started at a moment of a replay reserves.
struct Admitted {
    reserved: u64,
    /// What it takes of the budget before its job is known.
    starting: u64,
    /// What `reserved` goes by.
    basis: Basis,
    peak: u64,
    /// Millicores.
    cpu: u64,
}

/// Whether `one` and `other` are the same job, as `Key` tells them apart. A long queue asks this of every pair of its
/// jobs in every pass, too often for keys to be built.
fn same_job(one: &Ran, other: &Ran) -> bool {
    one.name == other.name && one.repository == other.repository && one.workflow == other.workflow
}

/// What a job's runner starts with at a moment of a replay.
struct Starting {
    /// What it takes of the budget before its job is known.
    memory: u64,
    /// What it reserves once it is.
    reserved: u64,
    /// What that goes by.
    basis: Basis,
    /// What it takes of the CPU budget, in millicores.
    cpu: u64,
}

/// Why a job did not start at a moment of a replay.
enum Refused {
    /// Every slot of its profile is taken.
    Slot,
    /// It does not fit the budget named.
    Budget(Short),
    /// It fits the slice's budget but not what the host's memory leaves it.
    Host(Short),
    /// The slice is at the high-water mark, though the job fits the budget.
    HighWater(Short),
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
    barriers: Barriers,
    /// What each pool's next job waited for, as the engine keeps it.
    pool_waits: BTreeMap<Pool, Waits>,
    pools: BTreeMap<Pool, PoolReplay>,
    /// The barrier the last pass ended with.
    barrier: Option<Barrier>,
    /// The room behind it, under `Barriers::On`.
    backfill: Option<Backfill<'run>>,
    /// Each pool's `Predictor::longest_usual` as this pass found it, read once as a plan reads it.
    usual: BTreeMap<Pool, Option<Duration>>,
    /// The jobs not arrived yet whose arrival is known, by when, as places in the queue.
    arriving: BTreeSet<(u64, usize)>,
    /// What arrives once a job ends, by the job.
    followers: BTreeMap<u64, Vec<Follower>>,
    /// The jobs started.
    started: Vec<u64>,
    runs: BTreeMap<RunKey, RunSpan>,
    backfilled: Backfilled,
    /// The moment replayed last, in ms.
    moment: u64,
    charged: u128,
    capacity: u128,
    barrier_for: u64,
    withheld: u128,
    memory_max: Option<u64>,
    mirror: u64,
    trace: Option<&'run Trace>,
    most_credit: u64,
    host_reserve: HostReserve,
    expects_credit: CreditRoom,
    /// Whether the larger margin stands for the next plan (`band_stands`).
    short: bool,
    /// When the next plan with no arrival or end is, in ms.
    next_plan: u64,
    /// What each pool's next runner starts with this pass, with a trace: a long queue asks it of every job, every plan.
    needs: RefCell<BTreeMap<Pool, Need>>,
    /// How many jobs the last pass left waiting for the slice's budget.
    held_for_slice: u64,
    /// How many it left waiting for the host's memory alone.
    held_for_host: u64,
    /// What each of those needs of the host.
    host_needs: Vec<u64>,
    band: Band,
    slice_waited: u64,
    host_waited: u64,
    hazard: Hazard,
    /// The credit at the last look.
    last_credit: u64,
    /// The host's credit at the last look.
    last_host_credit: u64,
    /// The sets of runners that ran when a job started, and the count that let it, while one of them runs.
    cohorts: Vec<Cohort>,
}

/// What a pool's next runner takes of the budgets before its job is known.
#[derive(Clone, Copy)]
struct Need {
    memory: u64,
    /// Millicores.
    cpu: u64,
}

/// The runners a start left running, and the count it went by.
struct Cohort {
    /// Their places in the jobs replayed.
    jobs: Vec<u64>,
    /// What they counted, in bytes.
    count: u64,
}

impl<'run> Replayer<'run> {
    /// Runs the replay to its end: each moment jobs arrive or end, a limit passes or the samples are read.
    fn play(mut self, queue: &[Queued<'run>]) -> Replay {
        let mut waiting: Vec<Queued<'_>> = Vec::new();
        loop {
            let arrival = self.arriving.first().map(|(arrives, _)| *arrives);
            let end = self.running.iter().map(|running| running.ends).min();
            let event = [arrival, end, self.next_limit()]
                .into_iter()
                .flatten()
                .min();
            let Some(now) = [event, self.next_look(&waiting)]
                .into_iter()
                .flatten()
                .min()
            else {
                break;
            };
            self.account(now);
            self.finish(now);
            if event != Some(now) && !self.plans_at(now) {
                continue;
            }
            self.rereserve(now);
            while let Some(&(arrived, at)) =
                self.arriving.first().filter(|(arrives, _)| *arrives <= now)
            {
                self.arriving.remove(&(arrived, at));
                waiting.push(Queued {
                    arrived,
                    ..queue[at]
                });
            }
            // By arrival, as IDs follow queue time in the daemon: a job that follows a delayed one queues after what arrived
            // meanwhile, though its ID in the history is older.
            waiting.sort_by_key(|queued| (queued.arrived, queued.job));
            self.pass(&mut waiting, now);
            self.note_worst(self.mirror);
        }
        self.replay(queue)
    }

    /// The next moment to read the samples, in ms, with a trace and something to read: a job that ran, or one waiting
    /// while the host's readings go on.
    fn next_look(&self, waiting: &[Queued<'run>]) -> Option<u64> {
        let trace = self.trace?;
        let waits = !waiting.is_empty() && self.moment < trace.ends();
        (!self.running.is_empty() || waits).then_some(self.moment + STEP)
    }

    /// Whether a plan with no arrival or end runs at `now`; if so, the next is a poll later.
    fn plans_at(&mut self, now: u64) -> bool {
        if now < self.next_plan {
            return false;
        }
        let poll = u64::try_from(self.config.host.poll_interval.as_millis()).unwrap_or(u64::MAX);
        self.next_plan = now.saturating_add(poll);
        true
    }

    /// The next moment a pool's wait reaches its limit, in ms.
    fn next_limit(&self) -> Option<u64> {
        self.pool_waits
            .values()
            .flat_map(Waits::deadlines)
            .map(time::ms)
            .filter(|limit| *limit > self.moment)
            .min()
    }

    /// Adds what ran from the last moment to `now`, which is what the last pass left.
    fn account(&mut self, now: u64) {
        let span = now.saturating_sub(self.moment);
        self.look(span);
        let taken = self.taken();
        self.charged += u128::from(taken) * u128::from(span);
        if let Some(budget) = self.budget {
            self.capacity += u128::from(budget) * u128::from(span);
            if self.barrier.is_some() {
                self.withheld += u128::from(budget.saturating_sub(taken)) * u128::from(span);
            }
        }
        if self.barrier.is_some() {
            self.barrier_for += span;
        }
        self.moment = now;
    }

    /// With a trace, notes what the last `span` ms held: the waits, the slice and the host against their lines, what
    /// started runners came to use, and what became of the credit.
    fn look(&mut self, span: u64) {
        let Some(trace) = self.trace else {
            return;
        };
        self.slice_waited += self.held_for_slice * span;
        self.host_waited += self.held_for_host * span;
        if self.running.is_empty() {
            self.last_credit = 0;
            self.last_host_credit = 0;
            return;
        }
        let slice = self.slice();
        let host_slice = self.used_now() + trace.host_overhead;
        let hazard = &mut self.hazard;
        hazard.slice_peak = hazard.slice_peak.max(slice);
        hazard.active += span;
        if let Some(max) = self.memory_max {
            let mark = max / 100 * HIGH_WATER;
            hazard.over_mark.note(slice.checked_sub(mark), span);
            hazard.over_max.note(slice.checked_sub(max), span);
        }
        if let Some(outside) = trace.outside_at(self.moment) {
            let available = trace
                .total
                .saturating_sub(outside)
                .saturating_sub(host_slice);
            hazard.under_throttling += span * u64::from(available < THROTTLES_UNDER);
            hazard.under_oomd += span * u64::from(available < OOMD_UNDER);
            hazard.in_band += span * u64::from(self.short);
            hazard.lowest_available = Some(
                hazard
                    .lowest_available
                    .map_or(available, |lowest| lowest.min(available)),
            );
        }
        let count = self.count();
        let hazard = &mut self.hazard;
        hazard.credit += u128::from(count.credit) * u128::from(span);
        hazard.most_credit = hazard.most_credit.max(count.credit);
        hazard.host_credit += u128::from(count.host_credit) * u128::from(span);
        let fell = count.credit < self.last_credit;
        hazard.credit_falls += u64::from(fell);
        if fell || count.host_credit < self.last_host_credit {
            let before = pooled::Count {
                credit: self.last_credit,
                host_credit: self.last_host_credit,
                ..count
            };
            self.hazard.overdrawn += u64::from(self.overdrawn(before, count));
        }
        self.last_credit = count.credit;
        self.last_host_credit = count.host_credit;
        self.note_unfunded(span);
    }

    /// Whether the fall from `before` to `now` left the count past a budget it was within: `MemoryMax` or the host's
    /// limit, each read as it reads the count.
    fn overdrawn(&self, before: pooled::Count, now: pooled::Count) -> bool {
        let max = self.memory_max.unwrap_or(u64::MAX);
        let host = self
            .host_memory()
            .map_or(u64::MAX, |memory| host::limit(&memory, self.short));
        (now.pooled() > max && before.pooled() <= max)
            || (now.hosted() > host && before.hosted() <= host)
    }

    /// Notes how far what any start left running uses now stands past the count that let it start.
    fn note_unfunded(&mut self, span: u64) {
        let used: BTreeMap<u64, u64> = self
            .running
            .iter()
            .map(|running| (running.job, self.used(running).unwrap_or(0)))
            .collect();
        self.cohorts
            .retain(|cohort| cohort.jobs.iter().any(|job| used.contains_key(job)));
        let all: u64 = used.values().sum();
        let lowest = self.cohorts.iter().map(|cohort| cohort.count).min();
        let mut worst = None;
        // What they all use bounds what any set of them does.
        if lowest.is_some_and(|lowest| all > lowest) {
            for cohort in &self.cohorts {
                let of_cohort: u64 = cohort.jobs.iter().filter_map(|job| used.get(job)).sum();
                worst = worst.max(of_cohort.checked_sub(cohort.count).filter(|past| *past > 0));
            }
        }
        self.hazard.unfunded.note(worst, span);
    }

    /// What `running` uses now, as its samples say; without a trace of it, or a sample of the moment, `None`.
    fn used(&self, running: &Running<'_>) -> Option<u64> {
        match running
            .trace?
            .at(self.moment.saturating_sub(running.started))
        {
            Reading::Used(memory) => Some(memory),
            Reading::NotYet | Reading::Stale => None,
        }
    }

    /// What the slice holds now as the mark reads it: its runners' use with the trace's overhead.
    fn slice(&self) -> u64 {
        self.used_now() + self.trace.map_or(0, |trace| trace.overhead)
    }

    /// What the guards read of the slice now, as a sample would show it at a decision.
    fn slice_read(&self) -> u64 {
        self.used_now() + self.trace.map_or(0, |trace| trace.guard_overhead)
    }

    /// What the running jobs use now, as their samples say.
    fn used_now(&self) -> u64 {
        self.running
            .iter()
            .map(|running| self.used(running).unwrap_or(0))
            .sum()
    }

    /// The host as a reading would have it now: its other programs as recorded, the slice as replayed. `None` without
    /// a trace of it.
    fn host_memory(&self) -> Option<HostMemory> {
        let trace = self.trace.filter(|trace| trace.total > 0)?;
        let outside = trace.outside_at(self.moment)?;
        let slice = self.used_now() + trace.host_overhead;
        Some(HostMemory {
            at: self.moment / 1000,
            total: trace.total,
            available: trace.total.saturating_sub(outside).saturating_sub(slice),
            slice,
            some: None,
            full: None,
            holders: Vec::new(),
            ceiling: None,
            swap: None,
        })
    }

    /// The aggregate count of what runs, the mirror's memory with it.
    fn count(&self) -> pooled::Count {
        let held: Vec<pooled::Runner<'_>> = self
            .running
            .iter()
            .map(|running| {
                let ran = self.moment.saturating_sub(running.started);
                let named = running.trace.is_none_or(|trace| ran >= trace.named_after);
                pooled::Runner {
                    key: named.then_some(&running.key),
                    takes: self.charge(running),
                    used: self
                        .used(running)
                        .filter(|_| running.run.measure == Measure::WithoutPageCache),
                    ran: Some(Duration::from_millis(ran)),
                    basis: Some(running.basis),
                }
            })
            .collect();
        let host = self.host_memory();
        pooled::count(&pooled::Snapshot {
            runners: &held,
            committed: self.mirror,
            slice: self.memory_max.map(|memory_max| pooled::Slice {
                memory: self.slice_read(),
                memory_max,
            }),
            host: host.map(|memory| {
                host::pooling(
                    &memory,
                    host::Remembered {
                        short: self.short,
                        stalled: false,
                    },
                )
            }),
            most: if self.trace.is_some() && self.policy == Policy::Measured {
                self.most_credit
            } else {
                0
            },
            host_reserve: self.trace.is_some()
                && self.policy == Policy::Measured
                && self.host_reserve == HostReserve::Top2,
        })
    }

    /// What the host leaves beside what runs, the mirror's memory counted; `None` without a trace of it.
    fn host_left(&self) -> Option<i64> {
        let memory = self.host_memory()?;
        let limit = host::limit(&memory, self.short && !self.running.is_empty());
        Some(host::left(limit, self.count().hosted(), false))
    }

    /// Serves `waiting` at `now`, as one `scheduler::plan` does: each job in turn starts if it fits, until a job that
    /// waited long enough puts up the barrier; the jobs behind it start if `Backfill` lets them, and still age by what
    /// they lack themselves.
    fn pass(&mut self, waiting: &mut Vec<Queued<'run>>, now: u64) {
        let at = instant(now);
        self.barrier = None;
        self.backfill = None;
        self.usual.clear();
        self.needs.borrow_mut().clear();
        self.held_for_slice = 0;
        self.held_for_host = 0;
        self.host_needs.clear();
        let listed = waiting.clone();
        let mut index = 0;
        while let Some(queued) = waiting.get(index).copied() {
            match self.admits(queued, waiting, now) {
                Ok(admitted) if self.barrier.is_none() => {
                    waiting.remove(index);
                    self.start(queued, &admitted, None, now);
                }
                Ok(admitted) => match self.passes(queued, &listed) {
                    Some(usual) => {
                        waiting.remove(index);
                        self.start(queued, &admitted, Some(usual), now);
                    }
                    None => index += 1,
                },
                Err(Refused::Slot) => index += 1,
                Err(Refused::Budget(short)) => {
                    if self.barrier.is_none() && short.scarce == Scarce::Cpu {
                        self.cpu_held += 1;
                    }
                    self.held_for_slice += u64::from(short.scarce == Scarce::Memory);
                    let first = self.barrier.is_none();
                    self.hold(queued.run, short, at);
                    if first && self.barrier.is_some() {
                        self.backfill_behind(queued.run, &waiting[..index], &listed, now);
                    }
                    index += 1;
                }
                Err(Refused::Host(short)) => {
                    self.held_for_host += 1;
                    // A job behind a barrier does not wait for the host, as the daemon's `Turn::behind`: the owner does.
                    if self.barrier.is_none() {
                        self.host_needs.push(short.memory);
                    }
                    let first = self.barrier.is_none();
                    self.hold(queued.run, short, at);
                    if first && self.barrier.is_some() {
                        self.backfill_behind(queued.run, &waiting[..index], &listed, now);
                    }
                    index += 1;
                }
                Err(Refused::HighWater(short)) => {
                    if self.barrier.is_none() {
                        self.held_back += 1;
                    }
                    self.hold(queued.run, short, at);
                    index += 1;
                }
            }
        }
        self.short = self.band_stands();
        self.settle_waits(waiting);
    }

    /// Whether the larger margin stands for the next pass.
    fn band_stands(&self) -> bool {
        match self.band {
            Band::AnyWaiter => self.held_for_host > 0,
            Band::NoWaiterFits => self.host_memory().is_some_and(|memory| {
                let reserved = self.count().hosted();
                host::short(&memory, reserved, false, self.host_needs.iter().copied())
            }),
        }
    }

    /// What the engine does after a plan: a pool with nothing `waiting` loses its served mark, and one with nothing left
    /// to keep goes.
    fn settle_waits(&mut self, waiting: &[Queued<'run>]) {
        let queued: BTreeSet<Pool> = waiting.iter().map(|queued| pool_of(queued.run)).collect();
        self.pool_waits.retain(|pool, waits| {
            if !queued.contains(pool) {
                waits.emptied();
            }
            !waits.is_empty()
        });
    }

    /// Unless `Barriers::Off`, ages the pool of `run`, which lacks what `short` says, and lets it put up the barrier, as
    /// `scheduler::plan` and the engine do. None ages at the high-water mark, read job by job as `admits` reads it:
    /// its zeros say nothing.
    fn hold(&mut self, run: &'run Ran, short: Short, at: SystemTime) {
        if self.barriers == Barriers::Off {
            return;
        }
        let drained = (!self.at_high_water()).then_some(self.drained());
        let waits = self.pool_waits.entry(pool_of(run)).or_default();
        if drained.is_some() {
            waits.note(short.scarce, short.job, at);
        }
        if self.barrier.is_none() {
            self.barrier = Barrier::of(short, waits, drained, at);
            if self.barrier.is_some() {
                waits.own(short.job);
            }
        }
    }

    /// Under `Barriers::On`, finds the room behind the barrier `run` just put up, `older` being the jobs left waiting
    /// before it and `listed` what the pass began with. A pool's jobs start alike, so its first speaks for it.
    fn backfill_behind(
        &mut self,
        run: &'run Ran,
        older: &[Queued<'run>],
        listed: &[Queued<'run>],
        now: u64,
    ) {
        let Some(barrier) = self.barrier.filter(|_| self.barriers == Barriers::On) else {
            return;
        };
        let mut pools = BTreeSet::new();
        let older: Vec<Older<'_>> = older
            .iter()
            .filter(|queued| self.config.profiles.contains_key(&queued.run.profile))
            .filter(|queued| pools.insert(pool_of(queued.run)))
            .map(|queued| {
                let starting = self.starting(*queued, listed, now);
                Older {
                    profile: &queued.run.profile,
                    free_slot: self.free_slots(&queued.run.profile) > 0,
                    memory: starting.memory,
                    cpu: starting.cpu,
                }
            })
            .collect();
        let runners = self.runners();
        let around = Around {
            left: self.left(),
            credit_room: self.credit_room(),
            host_credit_room: self.host_credit_room(),
            drained: self.drained(),
            runners: &runners,
            older: &older,
        };
        self.backfill = Backfill::behind(barrier, &run.profile, around, instant(now));
    }

    /// How much more the credit could take off what runs, as `Budget::credit_room`.
    fn credit_room(&self) -> u64 {
        if self.trace.is_none()
            || self.policy != Policy::Measured
            || self.expects_credit == CreditRoom::LeftOut
        {
            return 0;
        }
        self.most_credit.saturating_sub(self.count().credit)
    }

    /// How much more the host's credit could take off what runs, as `Budget::host_credit_room`.
    fn host_credit_room(&self) -> u64 {
        if self.expects_credit == CreditRoom::LeftOut {
            return 0;
        }
        self.count().host_credit_room
    }

    /// What a drain leaves.
    fn drained(&self) -> Drained {
        Drained {
            memory: self.budget.map(signed),
            cpu: self.cpu_budget.map(signed),
            host: self.host_memory().map(|memory| {
                let limit = host::limit(&memory, !self.running.is_empty());
                signed(limit).saturating_sub(signed(self.mirror))
            }),
            host_plain: self.host_memory().map(|memory| {
                signed(host::limit(&memory, false)).saturating_sub(signed(self.mirror))
            }),
        }
    }

    /// How many of `profile`'s slots are free.
    fn free_slots(&self, profile: &str) -> usize {
        let limits = &self.config.profiles[profile];
        let slots = self.slots.get(profile).copied().unwrap_or(limits.slots);
        let in_profile = self
            .running
            .iter()
            .filter(|running| running.profile == profile)
            .count();
        (slots as usize).saturating_sub(in_profile)
    }

    /// Whether `queued`, which fits, may start behind the barrier, `listed` being what the pass began with: the usual
    /// run time it passes by, if so.
    fn passes(&mut self, queued: Queued<'run>, listed: &[Queued<'run>]) -> Option<Duration> {
        let run = queued.run;
        self.backfill.as_ref()?;
        let predictor = &self.predictor;
        let usual = *self.usual.entry(pool_of(run)).or_insert_with(|| {
            let keys = listed
                .iter()
                .filter(|other| other.run.repository == run.repository)
                .filter(|other| other.run.profile == run.profile)
                .map(|other| Key::of(other.run));
            predictor.longest_usual(keys, &run.profile)
        });
        let candidate = Candidate {
            profile: &run.profile,
            free_slots: self.free_slots(&run.profile),
            usual,
            caps: self.caps(&run.profile),
        };
        let backfill = self.backfill.as_mut()?;
        backfill.admits(candidate).then_some(usual).flatten()
    }

    /// The budgets left beside what runs.
    fn left(&self) -> Left {
        let cpu: u64 = self.running.iter().map(|running| running.cpu).sum();
        let left = |budget: u64, taken: u64| signed(budget).saturating_sub(signed(taken));
        Left {
            memory: self.budget.map(|budget| left(budget, self.taken())),
            cpu: self.cpu_budget.map(|budget| left(budget, cpu)),
            host: self.host_left(),
        }
    }

    /// `profile`'s caps, its CPU at most the budget, as `scheduler::Profile` has them.
    fn caps(&self, profile: &str) -> Caps {
        let limits = &self.config.profiles[profile];
        let cpu = u64::from(limits.cpus).saturating_mul(1000);
        Caps {
            memory: limits.memory.0,
            cpu: self.cpu_budget.map_or(cpu, |budget| cpu.min(budget)),
        }
    }

    /// Notes the worst moments so far, with the `mirror`'s memory.
    fn note_worst(&mut self, mirror: u64) {
        self.most_running = self.most_running.max(self.running.len() as u64);
        let peaks: u64 = self.running.iter().map(|running| running.peak).sum();
        self.worst_overlap = self.worst_overlap.max(peaks + mirror);
        let cpu: u64 = self
            .running
            .iter()
            .map(|running| running.run.cpu_average.unwrap_or(0))
            .sum();
        self.worst_cpu = self.worst_cpu.max(cpu);
    }

    /// What the replay of `queue` came to.
    fn replay(mut self, queue: &[Queued<'run>]) -> Replay {
        let started: BTreeSet<u64> = self.started.iter().copied().collect();
        let never_started: Vec<&Queued<'_>> = queue
            .iter()
            .filter(|queued| !started.contains(&queued.job))
            .collect();
        for queued in &never_started {
            self.pools
                .entry(pool_of(queued.run))
                .or_default()
                .never_started += 1;
        }
        let waits = self.waits;
        let first = queue.first().map_or(0, |queued| queued.run.queued_at);
        Replay {
            jobs: queue.len() as u64,
            never_started: never_started.len() as u64,
            mean_wait: waits.iter().sum::<u64>() / (waits.len() as u64).max(1),
            wait: spread(waits),
            most_running: self.most_running,
            worst_overlap: self.worst_overlap,
            makespan: self.last_end.saturating_sub(first) / 1000,
            under_predicted: self.under_predicted,
            held_back: self.held_back,
            cpu_held: self.cpu_held,
            worst_cpu: self.worst_cpu,
            pools: self.pools,
            charged: self.charged,
            capacity: self.capacity,
            barrier_for: self.barrier_for,
            withheld: self.withheld,
            runs: self.runs.into_values().map(RunSpan::replay).collect(),
            backfilled: self.backfilled,
            slice_waited: self.slice_waited,
            host_waited: self.host_waited,
            hazard: self.hazard,
        }
    }

    /// What runs, as a backfill reads it: each job's usual run time as the runs finished so far say.
    fn runners(&self) -> Vec<Runner<'_>> {
        self.running
            .iter()
            .map(|running| Runner {
                profile: running.profile,
                started_at: instant(running.started),
                usual: self.predictor.usual(&Key::of(running.run), running.profile),
                memory: self.charge(running),
                cpu: running.cpu,
                caps: self.caps(running.profile),
            })
            .collect()
    }

    /// Whether measured admission starts nothing now: with a trace, what the slice holds is at the high-water mark, and
    /// without one the running jobs' peaks are.
    fn at_high_water(&self) -> bool {
        if self.trace.is_some() {
            return self.policy == Policy::Measured
                && self.memory_max.is_some_and(|max| {
                    self.slice_read().saturating_mul(100) >= max.saturating_mul(HIGH_WATER)
                });
        }
        let peaks: u64 = self.running.iter().map(|running| running.peak).sum();
        self.policy == Policy::Measured
            && self.budget.is_some_and(|budget| {
                peaks.saturating_mul(100) >= budget.saturating_mul(HIGH_WATER)
            })
    }

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
            if let Some(by) = ended.backfilled {
                let overrun = ended.ends.saturating_sub(by);
                self.backfilled.overran += u64::from(overrun > 0);
                self.backfilled.most_overrun = self.backfilled.most_overrun.max(overrun / 1000);
            }
            let span = self.runs.get_mut(&run_key(ended.run));
            if let Some(span) = span {
                span.last_end = span.last_end.max(ended.ends);
            }
            for follower in self.followers.remove(&ended.job).unwrap_or_default() {
                self.arriving
                    .insert((ended.ends + follower.gap, follower.at));
            }
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
            let prediction =
                self.predictor
                    .predict(&key, running.profile, limits.memory.0, now, copies);
            running.reserved = prediction.memory;
            running.basis = prediction.basis;
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

    /// What the running jobs take of the budget, as `pooled::count` adds them up, the mirror left out: the budget is
    /// what it leaves.
    fn taken(&self) -> u64 {
        self.count().pooled().saturating_sub(self.mirror)
    }

    /// What `running` takes of the budget: under measured admission, as the daemon counts it, the larger of what it
    /// reserved and what it uses, without a trace its peak for the whole run; with one, until its job is known what it
    /// started reserving, and at its cap while its samples say nothing.
    fn charge(&self, running: &Running<'_>) -> u64 {
        let Some(trace) = running.trace.filter(|_| self.policy == Policy::Measured) else {
            return match self.policy {
                Policy::Caps => running.reserved,
                Policy::Measured => running.reserved.max(running.peak),
            };
        };
        let ran = self.moment.saturating_sub(running.started);
        let reserved = if ran < trace.named_after {
            running.starting.max(running.reserved)
        } else {
            running.reserved
        };
        match trace.at(ran) {
            Reading::NotYet => reserved,
            Reading::Used(memory) => reserved.max(memory),
            Reading::Stale => self.config.profiles[running.profile].memory.0,
        }
    }

    /// What `queued` reserves if it starts at `now`, or why the policy does not admit it, `waiting` being every job
    /// not started yet.
    fn admits(
        &self,
        queued: Queued<'run>,
        waiting: &[Queued<'run>],
        now: u64,
    ) -> Result<Admitted, Refused> {
        let run = queued.run;
        let Some(limits) = self.config.profiles.get(&run.profile) else {
            return Err(Refused::Slot);
        };
        if self.free_slots(&run.profile) == 0 {
            return Err(Refused::Slot);
        }
        let starting = self.starting(queued, waiting, now);
        let short = |scarce, held| Short {
            job: queued.job,
            scarce,
            held,
            memory: starting.memory,
            cpu: starting.cpu,
        };
        if let Some(budget) = self.budget {
            if self.taken() + starting.memory > budget {
                return Err(Refused::Budget(short(Scarce::Memory, Held::ByBudget)));
            }
            if self.at_high_water() {
                return Err(Refused::HighWater(short(Scarce::Memory, Held::ByBudget)));
            }
        }
        if self
            .host_left()
            .is_some_and(|left| left < signed(starting.memory))
        {
            return Err(Refused::Host(short(Scarce::Memory, Held::ByHost)));
        }
        if let Some(cpu_budget) = self.cpu_budget {
            let taken: u64 = self.running.iter().map(|running| running.cpu).sum();
            if taken + starting.cpu > cpu_budget {
                return Err(Refused::Budget(short(Scarce::Cpu, Held::ByBudget)));
            }
        }
        Ok(Admitted {
            reserved: starting.reserved,
            starting: starting.memory,
            basis: starting.basis,
            // A job over its cap is killed at it.
            peak: run.memory_peak.min(limits.memory.0),
            cpu: self.cpu(&Key::of(run), &run.profile),
        })
    }

    /// What the runner of `queued` starts with. As in the daemon, it starts reserving the largest prediction among the
    /// waiting jobs of its repository's pool (those that ran in its profile), then its job's own.
    fn starting(&self, queued: Queued<'run>, waiting: &[Queued<'run>], now: u64) -> Starting {
        let run = queued.run;
        let profile = run.profile.as_str();
        let cap = self.config.profiles[profile].memory.0;
        let running = |job: &Ran| {
            self.running
                .iter()
                .any(|running| same_job(running.run, job))
        };
        // A waiting job with a copy waiting or running beside it, as the daemon sees its queue.
        let predicted = |job: &Ran| {
            let key = Key::of(job);
            let waiting = waiting
                .iter()
                .filter(|other| same_job(other.run, job))
                .count();
            let copies = if waiting > 1 || running(job) {
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
            let copies = if running(run) {
                Copies::Several
            } else {
                Copies::Alone
            };
            self.predictor.predict(&key, profile, cap, now, copies)
        };
        let pool = || {
            waiting
                .iter()
                .map(|other| other.run)
                .filter(|job| job.repository == run.repository && job.profile == run.profile)
        };
        let own = started();
        let need = || Need {
            memory: pool().map(predicted).max().unwrap_or(0),
            // As the daemon starts a runner: at the most CPU any waiting job of its repository's pool reserves.
            cpu: pool()
                .map(|job| self.cpu(&Key::of(job), profile))
                .max()
                .unwrap_or(0),
        };
        let need = if self.trace.is_some() {
            *self
                .needs
                .borrow_mut()
                .entry(pool_of(run))
                .or_insert_with(need)
        } else {
            need()
        };
        let (memory, reserved, basis) = match self.policy {
            Policy::Caps => (cap, cap, Basis::Cap),
            Policy::Measured => (need.memory.max(own.memory), own.memory, own.basis),
        };
        Starting {
            memory,
            reserved,
            basis,
            cpu: need.cpu.max(self.cpu(&Key::of(run), profile)),
        }
    }

    /// Starts `queued` at `now`, which ends its pool's wait; `usual`, the usual run time it started behind a barrier by.
    fn start(
        &mut self,
        queued: Queued<'run>,
        admitted: &Admitted,
        usual: Option<Duration>,
        now: u64,
    ) {
        let run = queued.run;
        let Some((profile, _)) = self.config.profiles.get_key_value(&run.profile) else {
            return;
        };
        let ran_for = run
            .finished_at
            .unwrap_or_default()
            .saturating_sub(run.started_at.unwrap_or_default());
        let waited = now.saturating_sub(queued.arrived) / 1000;
        let span = self.runs.entry(run_key(run)).or_insert(RunSpan {
            first_arrival: queued.arrived,
            last_end: now,
            profiles: BTreeSet::new(),
        });
        span.first_arrival = span.first_arrival.min(queued.arrived);
        span.profiles.insert(run.profile.clone());
        self.started.push(queued.job);
        self.backfilled.jobs += u64::from(usual.is_some());
        let pool = pool_of(run);
        if let Some(waits) = self.pool_waits.get_mut(&pool) {
            waits.started();
        }
        self.pools.entry(pool).or_default().waits.push(waited);
        self.waits.push(waited);
        self.running.push(Running {
            run,
            job: queued.job,
            profile,
            started: now,
            ends: now + ran_for,
            backfilled: usual.map(|usual| {
                now.saturating_add(u64::try_from(usual.as_millis()).unwrap_or(u64::MAX))
            }),
            reserved: admitted.reserved,
            peak: admitted.peak,
            cpu: admitted.cpu,
            key: Key::of(run),
            basis: admitted.basis,
            starting: admitted.starting,
            trace: self.trace.and_then(|trace| trace.runs.get(&RunId::of(run))),
        });
        self.needs.borrow_mut().clear();
        if self.trace.is_some() {
            self.cohorts.push(Cohort {
                jobs: self.running.iter().map(|running| running.job).collect(),
                count: self.taken(),
            });
        }
    }
}

/// `ms` since the epoch, as `scheduler::plan` reads the clock.
fn instant(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

/// A budget's bytes or millicores as `Drained` counts them.
fn signed(amount: u64) -> i64 {
    i64::try_from(amount).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
