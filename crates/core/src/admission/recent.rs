//! A job's recent runs, as admission and sizing read them (ADR 0070): each row of the history read once, in the order
//! the runs finished, into a window of the job's last successful runs, the hold a run at its cap puts on it, and the
//! runs past what they reserved. One rule for what each row says, whoever feeds it: the engine's predictor,
//! `simulate` and sizing. Pure: no I/O, no clock; it must not keep anything the rows do not say, so a restart that
//! reads the same rows decides the same.

use std::collections::{BTreeMap, VecDeque};

use super::Key;
use crate::sizing::{Capped, History, Measure, Ran};

/// Admission's window: a job's last this many successful runs, or `[sizing] min_jobs` when more. Under nearest rank
/// a p95 of fewer than 20 values is their largest; 20 is the fewest at which it can leave one outlier out.
pub const RECENT_RUNS: u64 = 20;

/// Sizing's window is this many times admission's: a cap too small kills a job, a reservation too small only lets
/// runs overlap, and a cold-cache run comes once in 20 to 50.
pub const SIZING_TIMES: u64 = 3;

/// A hold at the cap ends after this many successful runs in a row clear of their cap.
pub const CLEAN_RUNS: u32 = 3;

/// How a row ended, as the history keeps it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Completed, failed or lost: its peak counts.
    Finished,
    /// Cancelled: its peak may be cut short, so only a run that met its cap counts its peak.
    Cancelled,
}

/// A run in a job's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counted {
    /// When it finished, in ms.
    pub at: u64,
    /// Its peak, in bytes; at its cap or more when it met it, since the cap stopped it.
    pub peak: u64,
    /// Its CPU peak, millicores.
    pub cpu_peak: u64,
    /// The cap its container had.
    pub cap: u64,
    /// What it says about that cap.
    pub capped: Capped,
    /// Whether it completed with success.
    pub succeeded: bool,
}

/// The latest run of a job that met its cap or was killed for memory, and how many successful runs clear of their
/// cap came in a row after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// When that run finished, in ms.
    pub at: u64,
    /// The clear successful runs after it, in a row.
    pub clean: u32,
}

/// A run that peaked past what it reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overrun {
    /// When it finished, in ms.
    pub at: u64,
    /// Its peak.
    pub peak: u64,
}

/// One job's runs in one profile.
#[derive(Debug, Clone)]
pub struct JobRuns {
    /// How many successful runs its window holds.
    size: u64,
    window: VecDeque<Counted>,
    successes: u64,
    sorted: Vec<u64>,
    hold: Option<Hold>,
    overruns: Vec<Overrun>,
}

impl JobRuns {
    fn new(size: u64) -> JobRuns {
        JobRuns {
            size,
            window: VecDeque::new(),
            successes: 0,
            sorted: Vec::new(),
            hold: None,
            overruns: Vec::new(),
        }
    }

    /// The window's runs, oldest first.
    pub fn window(&self) -> impl Iterator<Item = &Counted> {
        self.window.iter()
    }

    /// How many runs the window has.
    pub fn runs(&self) -> u64 {
        self.window.len() as u64
    }

    /// How many of them succeeded.
    pub fn succeeded(&self) -> u64 {
        self.successes
    }

    /// Their peaks, sorted.
    pub fn peaks(&self) -> &[u64] {
        &self.sorted
    }

    /// The largest of them; 0 for none.
    pub fn max(&self) -> u64 {
        self.sorted.last().copied().unwrap_or(0)
    }

    /// The hold at the cap, if it has not ended by `CLEAN_RUNS` clear runs; `for_ms` ends it that long after its
    /// run as well, `None` never.
    pub fn held(&self, now: u64, for_ms: Option<u64>) -> Option<Hold> {
        self.hold.filter(|hold| {
            hold.clean < CLEAN_RUNS && for_ms.is_none_or(|ms| now < hold.at.saturating_add(ms))
        })
    }

    /// The runs past what they reserved that finished at or after the window's start and less than `for_ms` before
    /// `now`.
    pub fn overruns(&self, now: u64, for_ms: u64) -> impl Iterator<Item = &Overrun> {
        let start = self.start();
        self.overruns
            .iter()
            .filter(move |overrun| overrun.at >= start && now < overrun.at.saturating_add(for_ms))
    }

    /// When the window starts: its oldest run's finish once it is full, 0 before.
    fn start(&self) -> u64 {
        if self.successes >= self.size {
            self.window.front().map_or(0, |run| run.at)
        } else {
            0
        }
    }

    fn push(&mut self, run: Counted) {
        let size = self.size;
        self.successes += u64::from(run.succeeded);
        self.window.push_back(run);
        while self.successes > size {
            if self.window.pop_front().is_some_and(|old| old.succeeded) {
                self.successes -= 1;
            }
        }
        // A full window starts at its oldest successful run: failed runs before it never hold a place.
        while self.successes == size && self.window.front().is_some_and(|old| !old.succeeded) {
            self.window.pop_front();
        }
        self.sorted = self.window.iter().map(|run| run.peak).collect();
        self.sorted.sort_unstable();
        let start = self.start();
        self.overruns.retain(|overrun| overrun.at >= start);
    }
}

/// A job in one profile: a job of one name may run in several pools of its repository, whose runs say nothing of
/// each other's (ADR 0058).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InProfile {
    /// The job.
    pub key: Key,
    /// The profile it ran in.
    pub profile: String,
}

/// Every job's recent runs, with a window of `size` successful runs.
#[derive(Debug, Clone)]
pub struct Recent {
    size: u64,
    jobs: BTreeMap<InProfile, JobRuns>,
    /// Each profile's jobs' windows together, sorted.
    profiles: BTreeMap<String, Vec<u64>>,
}

impl Recent {
    /// Nothing read yet, with windows of `size` successful runs, at least 1.
    pub fn new(size: u64) -> Recent {
        Recent {
            size: size.max(1),
            jobs: BTreeMap::new(),
            profiles: BTreeMap::new(),
        }
    }

    /// Reads a row of `profile` that ended at `at` (ms) under `cap`. Rows must come in the order they finished.
    ///
    /// A run sampled with page cache counted adds no peak, and only a kill for memory says anything of its cap. A
    /// cancelled run adds its peak only when it met its cap: it got that far. A run past what it reserved is an
    /// overrun, cancelled or not. A successful run clear of its cap and of what it reserved counts toward the end of
    /// a hold; one that went past what it reserved starts the count again.
    pub fn read(&mut self, run: &Ran, profile: &str, ending: Ending, cap: u64, at: u64) {
        let size = self.size;
        // A run sampled with page cache counted says nothing of its job but a kill for memory.
        if run.measure == Measure::WithActiveFile && !run.oom_killed {
            return;
        }
        let job = self
            .jobs
            .entry(InProfile {
                key: Key::of(run),
                profile: profile.to_owned(),
            })
            .or_insert_with(|| JobRuns::new(size));
        if run.measure == Measure::WithActiveFile {
            job.hold = Some(Hold { at, clean: 0 });
            return;
        }
        let capped = run.capped(cap);
        let overran = run
            .reserved
            .is_some_and(|reserved| run.memory_peak > reserved);
        if capped.holds() {
            job.hold = Some(Hold { at, clean: 0 });
        } else if ending == Ending::Finished
            && run.succeeded
            && let Some(hold) = &mut job.hold
        {
            hold.clean = if overran { 0 } else { hold.clean + 1 };
        }
        if overran {
            job.overruns.push(Overrun {
                at,
                peak: run.memory_peak,
            });
        }
        if ending == Ending::Finished || capped.met() {
            job.push(Counted {
                at,
                peak: if capped.met() {
                    run.memory_peak.max(cap)
                } else {
                    run.memory_peak
                },
                cpu_peak: run.cpu_peak,
                cap,
                capped,
                succeeded: ending == Ending::Finished && run.succeeded,
            });
            let mut peaks: Vec<u64> = self
                .jobs
                .iter()
                .filter(|(job, _)| job.profile == profile)
                .flat_map(|(_, runs)| runs.sorted.iter().copied())
                .collect();
            peaks.sort_unstable();
            self.profiles.insert(profile.to_owned(), peaks);
        }
    }

    /// Reads every row of `history` in the order they finished, each under the cap its container had, else
    /// `cap_of(profile)`, else none.
    pub fn read_history(&mut self, history: &History, cap_of: impl Fn(&str) -> Option<u64>) {
        for (run, ending) in rows(history) {
            let cap = run
                .memory_cap
                .or_else(|| cap_of(&run.profile))
                .unwrap_or(u64::MAX);
            self.read(run, &run.profile, ending, cap, finished(run));
        }
    }

    /// The runs of `key` in `profile`, if it was seen.
    pub fn job(&self, key: &Key, profile: &str) -> Option<&JobRuns> {
        self.jobs.get(&InProfile {
            key: key.clone(),
            profile: profile.to_owned(),
        })
    }

    /// Every job of `profile` and its runs.
    pub fn jobs_of<'recent>(
        &'recent self,
        profile: &'recent str,
    ) -> impl Iterator<Item = (&'recent Key, &'recent JobRuns)> {
        self.jobs
            .iter()
            .filter(move |(job, _)| job.profile == profile)
            .map(|(job, runs)| (&job.key, runs))
    }

    /// `profile`'s jobs' windows together, sorted.
    pub fn profile(&self, profile: &str) -> &[u64] {
        self.profiles.get(profile).map_or(&[], Vec::as_slice)
    }
}

/// When a row ended, as the windows order it: its finish, or its queueing without one.
pub fn finished(run: &Ran) -> u64 {
    run.finished_at.unwrap_or(run.queued_at)
}

/// `history`'s rows, finished and cancelled, in the order they finished; ties in the store's order, finished ones
/// first.
pub fn rows(history: &History) -> Vec<(&Ran, Ending)> {
    let mut rows: Vec<(&Ran, Ending)> = history
        .jobs
        .iter()
        .map(|run| (run, Ending::Finished))
        .chain(history.cancelled.iter().map(|run| (run, Ending::Cancelled)))
        .collect();
    rows.sort_by_key(|(run, _)| finished(run));
    rows
}
