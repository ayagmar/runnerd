//! Admission by measured peaks: a job's memory peak and average CPU predicted from the job history (`Predictor`),
//! and the history replayed through admission by caps and by predictions, with or without a CPU budget
//! (`simulate`), to show what each would have done with the same jobs, with the barrier `scheduler::plan` puts up for
//! a starved job or without, and with jobs arriving when they did or after what they followed; and what the host's
//! memory leaves runners (`host`). A job's memory and usual run time go by its last runs (`recent`, ADR 0070, 0097).
//! Pure: no I/O, no clock.

pub mod host;
pub mod recent;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use crate::api::Spread;
use crate::api::{Basis, JobPeak};
use crate::config::{self, Config};
use crate::scheduler::priority::{Around, Backfill, Candidate, Caps, Older, Runner, Short};
use crate::scheduler::{Barrier, Drained, Left, Pool, Scarce, Waits};
use crate::sizing::{GRANULE, History, Ran, spread};
use crate::time;
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
}

impl From<Barriers> for Replaying<'_> {
    fn from(barriers: Barriers) -> Self {
        Replaying {
            barriers,
            arrivals: Arrivals::AsQueued,
            known: None,
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
    let mut replayer = Replayer {
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
    };
    let mut waiting: Vec<Queued<'_>> = Vec::new();
    loop {
        let arrival = replayer.arriving.first().map(|(arrives, _)| *arrives);
        let end = replayer.running.iter().map(|running| running.ends).min();
        let Some(now) = [arrival, end, replayer.next_limit()]
            .into_iter()
            .flatten()
            .min()
        else {
            break;
        };
        replayer.account(now);
        replayer.finish(now);
        replayer.rereserve(now);
        while let Some(&(arrived, at)) = replayer
            .arriving
            .first()
            .filter(|(arrives, _)| *arrives <= now)
        {
            replayer.arriving.remove(&(arrived, at));
            waiting.push(Queued {
                arrived,
                ..queue[at]
            });
        }
        // By arrival, as IDs follow queue time in the daemon: a job that follows a delayed one queues after what arrived
        // meanwhile, though its ID in the history is older.
        waiting.sort_by_key(|queued| (queued.arrived, queued.job));
        replayer.pass(&mut waiting, now);
        replayer.note_worst(mirror);
    }
    replayer.replay(&queue)
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
    /// What it takes of the CPU budget, in millicores.
    cpu: u64,
}

/// Why a job did not start at a moment of a replay.
enum Refused {
    /// Every slot of its profile is taken.
    Slot,
    /// It does not fit the budget named.
    Budget(Short),
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
}

impl<'run> Replayer<'run> {
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

    /// Serves `waiting` at `now`, as one `scheduler::plan` does: each job in turn starts if it fits, until a job that
    /// waited long enough puts up the barrier; the jobs behind it start if `Backfill` lets them, and still age by what
    /// they lack themselves.
    fn pass(&mut self, waiting: &mut Vec<Queued<'run>>, now: u64) {
        let at = instant(now);
        self.barrier = None;
        self.backfill = None;
        self.usual.clear();
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
        self.settle_waits(waiting);
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
            drained: self.drained(),
            runners: &runners,
            older: &older,
        };
        self.backfill = Backfill::behind(barrier, &run.profile, around, instant(now));
    }

    /// What a drain leaves.
    fn drained(&self) -> Drained {
        Drained {
            memory: self.budget.map(signed),
            cpu: self.cpu_budget.map(signed),
            host: None,
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
            host: None,
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

    /// Whether measured admission starts nothing now: the running jobs' peaks are at the high-water mark.
    fn at_high_water(&self) -> bool {
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
            .map(|running| self.charge(running))
            .sum()
    }

    /// What `running` takes of the budget.
    fn charge(&self, running: &Running<'_>) -> u64 {
        match self.policy {
            Policy::Caps => running.reserved,
            Policy::Measured => running.reserved.max(running.peak),
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
        let short = |scarce| Short {
            job: queued.job,
            scarce,
            memory: starting.memory,
            cpu: starting.cpu,
        };
        if let Some(budget) = self.budget {
            if self.taken() + starting.memory > budget {
                return Err(Refused::Budget(short(Scarce::Memory)));
            }
            if self.at_high_water() {
                return Err(Refused::HighWater(short(Scarce::Memory)));
            }
        }
        if let Some(cpu_budget) = self.cpu_budget {
            let taken: u64 = self.running.iter().map(|running| running.cpu).sum();
            if taken + starting.cpu > cpu_budget {
                return Err(Refused::Budget(short(Scarce::Cpu)));
            }
        }
        Ok(Admitted {
            reserved: starting.reserved,
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
            self.predictor
                .predict(&key, profile, cap, now, copies)
                .memory
        };
        let pool = || {
            waiting
                .iter()
                .map(|other| other.run)
                .filter(|job| job.repository == run.repository && job.profile == run.profile)
        };
        let (memory, reserved) = match self.policy {
            Policy::Caps => (cap, cap),
            Policy::Measured => (
                pool().map(predicted).max().unwrap_or(0).max(started()),
                started(),
            ),
        };
        // As the daemon starts a runner: at the most CPU any waiting job of its repository's pool reserves.
        let cpu = pool()
            .map(|job| self.cpu(&Key::of(job), profile))
            .max()
            .unwrap_or(0)
            .max(self.cpu(&Key::of(run), profile));
        Starting {
            memory,
            reserved,
            cpu,
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
        });
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
