//! What really happened to a window of jobs, for a replay that runs them as they ran (`admission::simulate`, ADR
//! 0099): each run's samples, when its job became known, what the host's other programs held, what the slice holds
//! beside its runners; and what such a replay found of the hazards a pooled count brings. Pure: the store reads it.

use std::collections::BTreeMap;

use crate::admission::Key;
use crate::sizing::Ran;

/// How far apart a replay reads a run's samples, in ms: the sampler's interval.
pub const STEP: u64 = 2_000;

/// A run's sample older than this says nothing, as `Engine::takes` reads one, in ms.
const FRESH: u64 = 3 * STEP;

/// `MemAvailable` under which socket throttling starts on the host studied, in bytes (ADR 0099).
pub const THROTTLES_UNDER: u64 = 20 << 30;

/// `MemAvailable` under which systemd-oomd may act once swap is full too, in bytes: 90% used of the host studied.
pub const OOMD_UNDER: u64 = 6_657_199_309;

/// A run as a trace tells them apart: `Ran` keeps no job ID, and a run attempt names its jobs apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunId {
    /// Its workflow run on GitHub.
    pub run_id: u64,
    /// Which attempt.
    pub run_attempt: u32,
    /// The job's name.
    pub name: String,
}

impl RunId {
    /// The ID of `run`.
    pub fn of(run: &Ran) -> RunId {
        RunId {
            run_id: run.run_id,
            run_attempt: run.run_attempt,
            name: run.name.clone(),
        }
    }
}

/// A sample of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    /// In ms from its job's start.
    pub at: u64,
    /// What it used, in bytes.
    pub memory: u64,
}

/// What a run's samples say at a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// Before its first sample.
    NotYet,
    /// Its last sample is too old to say, as when the sampler could not read it.
    Stale,
    /// What it uses.
    Used(u64),
}

/// A run as it ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTrace {
    /// Its samples, oldest first.
    pub samples: Vec<Point>,
    /// How long after its job's start the daemon could name it, in ms: a job no listing showed has no name until
    /// GitHub's details do.
    pub named_after: u64,
}

impl RunTrace {
    /// What its samples say `ran` ms into its job. Past the last sample it holds at that one: a run's end is not a gap.
    pub fn at(&self, ran: u64) -> Reading {
        let next = self.samples.partition_point(|point| point.at <= ran);
        let Some(last) = next.checked_sub(1).map(|at| self.samples[at]) else {
            return Reading::NotYet;
        };
        if next < self.samples.len() && ran.saturating_sub(last.at) > FRESH {
            return Reading::Stale;
        }
        Reading::Used(last.memory)
    }

    fn peak(&self) -> u64 {
        self.samples
            .iter()
            .map(|point| point.memory)
            .max()
            .unwrap_or(0)
    }
}

/// What the host's other programs held from a moment on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outside {
    /// When, in ms.
    pub at: u64,
    /// What they held, in bytes (`HostMemory::outside`).
    pub memory: u64,
}

/// A window of jobs as they ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trace {
    /// The runs.
    pub runs: BTreeMap<RunId, RunTrace>,
    /// What the host's other programs held, oldest first. The readings do not react to what a replay runs: a fuller
    /// slice would make a real desktop swap more.
    pub outside: Vec<Outside>,
    /// The host's memory, in bytes; 0 with no reading, and then the replay has no host bound.
    pub total: u64,
    /// What the slice holds beside its runners' samples, in bytes: file cache read twice, seeds, the mirror. It cannot
    /// follow reordered jobs, so a replay carries one constant, chosen high (`store::trace_at`).
    pub overhead: u64,
    /// What the host's reading of the slice holds beside its runners' samples, in bytes: without page cache and
    /// reclaimable slab, so far less than `overhead`, and what `MemAvailable` is read from.
    pub host_overhead: u64,
}

impl Trace {
    /// What the host's other programs held at `at` (ms): the last reading then, else the first.
    pub fn outside_at(&self, at: u64) -> Option<u64> {
        let next = self.outside.partition_point(|point| point.at <= at);
        let point = self.outside.get(next.saturating_sub(1))?;
        Some(point.memory)
    }

    /// When the host's last reading was, in ms.
    pub fn ends(&self) -> u64 {
        self.outside.last().map_or(0, |point| point.at)
    }

    /// The same window with every run of a job given the samples of that job's heaviest run: a dependency bump that
    /// leaves every cache cold at once, whatever the run's own history was.
    #[must_use]
    pub fn cold(&self, jobs: &[Ran]) -> Trace {
        let mut heaviest: BTreeMap<Key, &RunTrace> = BTreeMap::new();
        for run in jobs {
            let Some(trace) = self.runs.get(&RunId::of(run)) else {
                continue;
            };
            let known = heaviest.entry(Key::of(run)).or_insert(trace);
            if trace.peak() > known.peak() {
                *known = trace;
            }
        }
        let mut cold = self.clone();
        for run in jobs {
            if let (Some(trace), Some(heavy)) = (
                cold.runs.get_mut(&RunId::of(run)),
                heaviest.get(&Key::of(run)),
            ) {
                trace.samples.clone_from(&heavy.samples);
            }
        }
        cold
    }
}

/// Spans in which a figure stood past a line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Episodes {
    /// How many.
    pub count: u64,
    /// How long, in all, in ms.
    pub time: u64,
    /// The furthest past the line, in bytes.
    pub worst: u64,
    open: bool,
}

impl Episodes {
    /// Notes `span` ms with the figure `past` the line by that much, if it is.
    pub(super) fn note(&mut self, past: Option<u64>, span: u64) {
        let Some(past) = past else {
            self.open = false;
            return;
        };
        self.count += u64::from(!self.open);
        self.open = true;
        self.time += span;
        self.worst = self.worst.max(past);
    }
}

/// What a replay with a `Trace` found of the risks of the count it ran by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hazard {
    /// The most the slice held, overhead included, in bytes.
    pub slice_peak: u64,
    /// The slice past `HIGH_WATER` percent of `MemoryMax`.
    pub over_mark: Episodes,
    /// The slice past `MemoryMax`.
    pub over_max: Episodes,
    /// What a set of runners used at once past the count that let the last of them start.
    pub unfunded: Episodes,
    /// How long, in ms, with a runner running, the host's `MemAvailable` stood under `THROTTLES_UNDER`.
    pub under_throttling: u64,
    /// How long it stood under `OOMD_UNDER`.
    pub under_oomd: u64,
    /// How long a runner ran, in ms: what those two are shares of.
    pub active: u64,
    /// The least the host had available with a runner running, in bytes.
    pub lowest_available: Option<u64>,
    /// How many times the credit fell.
    pub credit_falls: u64,
    /// How many of those left the count past a budget it was within.
    pub overdrawn: u64,
    /// The credit over the replay, in byte-milliseconds.
    pub credit: u128,
    /// The most it was, in bytes.
    pub most_credit: u64,
}
