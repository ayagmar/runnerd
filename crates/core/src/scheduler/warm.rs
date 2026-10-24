//! How many warm runners each repository wants (ADR 0050): its `warm` while it is active, by its jobs of the last hour
//! and the hours of the day its jobs usually come, plus the jobs its workflow runs seen so far are expected to queue
//! still, from the history. Pure: the engine reads the history and the clock and passes them in.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::IDLE_TIMEOUT;

/// How long after a repository's last job it stays active.
pub const ACTIVE_AFTER: Duration = Duration::from_secs(3600);

/// How far back the history is read, in milliseconds: 14 days.
pub const LOOKBACK: u64 = 14 * DAY;

/// An hour of the day is a repository's usual one when its jobs were queued in it on this many days of `LOOKBACK`.
const USUAL_DAYS: usize = 3;

/// A workflow's runs before its jobs per run are expected of the next.
const MIN_RUNS: usize = 3;

/// How long a run is remembered after its first job, whether or not it still counts.
const FORGET_AFTER: Duration = Duration::from_hours(24);

/// How long a run whose workflow is not known yet counts.
const UNKNOWN_SPAN: Duration = Duration::from_secs(3600);

const HOUR: u64 = 3600 * 1000;

/// A run as the history tells runs apart: repository, workflow, run ID and attempt.
type RunKey<'seen> = (&'seen str, &'seen str, u64, u32);
const DAY: u64 = 24 * HOUR;

/// A job the history saw queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name, when known.
    pub workflow: Option<String>,
    /// The workflow run it belongs to.
    pub run_id: u64,
    /// 1 for the run's first attempt.
    pub run_attempt: u32,
    /// When GitHub queued it, in milliseconds since the epoch.
    pub queued_at: u64,
}

/// What a workflow's runs are expected to queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Burst {
    /// The median number of jobs per run.
    pub jobs: usize,
    /// The 90th percentile of the time from a run's first job queued to its last.
    pub span: Duration,
}

/// What warm runners learn from the history: when each repository's jobs come, and how many jobs its workflows'
/// runs queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Learned {
    /// The hours of the day (UTC) each repository's jobs usually come in.
    usual: BTreeMap<String, BTreeSet<u64>>,
    /// When each repository's last job was queued, in milliseconds.
    last: BTreeMap<String, u64>,
    /// By repository and workflow.
    bursts: BTreeMap<(String, String), Burst>,
}

impl Learned {
    /// Learns from the jobs queued in the `LOOKBACK` before `now` (ms).
    pub fn new(seen: &[Seen], now: u64) -> Learned {
        let since = now.saturating_sub(LOOKBACK);
        let mut learned = Learned::default();
        let mut days: BTreeMap<(&str, u64), BTreeSet<u64>> = BTreeMap::new();
        // Each run's jobs, and when its first and last were queued.
        let mut runs: BTreeMap<RunKey<'_>, (usize, u64, u64)> = BTreeMap::new();
        for job in seen.iter().filter(|job| job.queued_at >= since) {
            learned.note(&job.repository, job.queued_at);
            days.entry((&job.repository, job.queued_at % DAY / HOUR))
                .or_default()
                .insert(job.queued_at / DAY);
            if let Some(workflow) = &job.workflow {
                let run = runs
                    .entry((&job.repository, workflow, job.run_id, job.run_attempt))
                    .or_insert((0, job.queued_at, job.queued_at));
                run.0 += 1;
                run.1 = run.1.min(job.queued_at);
                run.2 = run.2.max(job.queued_at);
            }
        }
        for ((repository, hour), days) in days {
            if days.len() >= USUAL_DAYS {
                learned
                    .usual
                    .entry(repository.to_owned())
                    .or_default()
                    .insert(hour);
            }
        }
        let mut workflows: BTreeMap<(&str, &str), Vec<(usize, u64)>> = BTreeMap::new();
        for ((repository, workflow, ..), (jobs, first, last)) in runs {
            workflows
                .entry((repository, workflow))
                .or_default()
                .push((jobs, last - first));
        }
        for ((repository, workflow), runs) in workflows {
            if runs.len() < MIN_RUNS {
                continue;
            }
            let mut jobs: Vec<usize> = runs.iter().map(|(jobs, _)| *jobs).collect();
            let mut spans: Vec<u64> = runs.iter().map(|(_, span)| *span).collect();
            jobs.sort_unstable();
            spans.sort_unstable();
            learned.bursts.insert(
                (repository.to_owned(), workflow.to_owned()),
                Burst {
                    jobs: jobs[(jobs.len() - 1) / 2],
                    span: Duration::from_millis(spans[(spans.len() * 90).div_ceil(100) - 1]),
                },
            );
        }
        learned
    }

    /// Keeps the last jobs `older` noted that this one has not read yet.
    pub fn absorb(&mut self, older: &Learned) {
        for (repository, at) in &older.last {
            self.note(repository, *at);
        }
    }

    /// Notes a job of `repository` queued or taken at `at` (ms).
    pub fn note(&mut self, repository: &str, at: u64) {
        let last = self.last.entry(repository.to_owned()).or_default();
        *last = (*last).max(at);
    }

    /// Whether `repository` is active at `now` (ms): its last job came within `ACTIVE_AFTER`, or `now` is in an hour
    /// of the day its jobs usually come in.
    pub fn active(&self, repository: &str, now: u64) -> bool {
        let after = u64::try_from(ACTIVE_AFTER.as_millis()).unwrap_or(u64::MAX);
        self.last
            .get(repository)
            .is_some_and(|last| now.saturating_sub(*last) < after)
            || self
                .usual
                .get(repository)
                .is_some_and(|hours| hours.contains(&(now % DAY / HOUR)))
    }

    /// What runs of `workflow` in `repository` are expected to queue, once it has `MIN_RUNS` runs.
    pub fn burst(&self, repository: &str, workflow: &str) -> Option<Burst> {
        self.bursts
            .get(&(repository.to_owned(), workflow.to_owned()))
            .copied()
    }
}

/// A workflow run seen lately, with the jobs of it seen so far.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    repository: String,
    workflow: Option<String>,
    jobs: BTreeSet<u64>,
    /// When its first job was queued or seen, in milliseconds.
    first: u64,
}

/// An attempt of a workflow run: a re-run keeps the run's ID, with the next attempt, and queues its jobs anew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Attempt {
    /// The workflow run's ID.
    pub run_id: u64,
    /// 1 for the run's first attempt.
    pub run_attempt: u32,
}

/// The workflow runs seen lately, by attempt: their jobs not queued yet get warm runners.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runs(BTreeMap<Attempt, Run>);

impl Runs {
    /// Notes `job` of `attempt` in `repository`, queued or taken at `at` (ms), and its workflow when known.
    pub fn note(
        &mut self,
        repository: &str,
        attempt: Attempt,
        job: u64,
        workflow: Option<&str>,
        at: u64,
    ) {
        let run = self.0.entry(attempt).or_insert_with(|| Run {
            repository: repository.to_owned(),
            workflow: None,
            jobs: BTreeSet::new(),
            first: at,
        });
        run.jobs.insert(job);
        run.first = run.first.min(at);
        if run.workflow.is_none() {
            run.workflow = workflow.map(str::to_owned);
        }
    }

    /// Moves `job` to `attempt` once GitHub said which attempt it is of, when it was noted under another of the
    /// run's: a job a runner took is first noted under its run's latest attempt known, which for a re-run's first
    /// job is the attempt before. An attempt it alone made is forgotten, and its time goes with the job; a job that
    /// joined an older attempt counts from `at` (ms). A job not followed stays so, so a run forgotten while its job
    /// ran is not counted again.
    pub fn place(
        &mut self,
        repository: &str,
        attempt: Attempt,
        job: u64,
        workflow: Option<&str>,
        at: u64,
    ) {
        // When the job was seen: an attempt it alone made began with it, while one it joined began before it.
        let mut seen: Option<u64> = None;
        let mut emptied = Vec::new();
        for (other, run) in self.attempts_mut(attempt.run_id) {
            if *other != attempt && run.jobs.remove(&job) {
                if run.jobs.is_empty() {
                    emptied.push(*other);
                    seen = Some(run.first.min(seen.unwrap_or(at)));
                } else {
                    seen = Some(seen.unwrap_or(at));
                }
            }
        }
        for other in emptied {
            self.0.remove(&other);
        }
        if let Some(seen) = seen {
            self.note(repository, attempt, job, workflow, seen);
        }
    }

    /// Names the workflow of run `run_id`'s attempts followed whose workflow was not known.
    pub fn name(&mut self, run_id: u64, workflow: &str) {
        for (_, run) in self.attempts_mut(run_id) {
            if run.workflow.is_none() {
                run.workflow = Some(workflow.to_owned());
            }
        }
    }

    /// The latest attempt of run `run_id` followed, or its first.
    pub fn latest(&self, run_id: u64) -> Attempt {
        self.0
            .range(
                Attempt {
                    run_id,
                    run_attempt: 0,
                }..=Attempt {
                    run_id,
                    run_attempt: u32::MAX,
                },
            )
            .next_back()
            .map_or(
                Attempt {
                    run_id,
                    run_attempt: 1,
                },
                |(attempt, _)| *attempt,
            )
    }

    fn attempts_mut(&mut self, run_id: u64) -> impl Iterator<Item = (&Attempt, &mut Run)> {
        self.0.range_mut(
            Attempt {
                run_id,
                run_attempt: 0,
            }..=Attempt {
                run_id,
                run_attempt: u32::MAX,
            },
        )
    }

    /// Forgets the runs whose first job came `FORGET_AFTER` before `now` (ms). One that stopped counting is kept
    /// until then, so a job of it seen again, as after a restart, does not count it again as a new run.
    pub fn expire(&mut self, now: u64) {
        let after = u64::try_from(FORGET_AFTER.as_millis()).unwrap_or(u64::MAX);
        self.0
            .retain(|_, run| now.saturating_sub(run.first) < after);
    }

    /// The jobs `repository`'s runs are expected to queue still at `now` (ms): per run that still counts, its
    /// workflow's jobs per run less the jobs of it seen.
    pub fn remaining(&self, repository: &str, learned: &Learned, now: u64) -> usize {
        self.0
            .values()
            .filter(|run| run.repository == repository && run.counts(learned, now))
            .filter_map(|run| {
                let burst = learned.burst(repository, run.workflow.as_deref()?)?;
                Some(burst.jobs.saturating_sub(run.jobs.len()))
            })
            .sum()
    }
}

impl Run {
    /// Whether it still counts at `now` (ms): until its workflow's span plus `IDLE_TIMEOUT` after its first job.
    fn counts(&self, learned: &Learned, now: u64) -> bool {
        let span = self
            .workflow
            .as_deref()
            .and_then(|workflow| learned.burst(&self.repository, workflow))
            .map_or(UNKNOWN_SPAN, |burst| burst.span + IDLE_TIMEOUT);
        now.saturating_sub(self.first) < u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
    }
}

/// How many warm runners a repository with `warm` configured wants: `warm` while it is `active`, plus the jobs its
/// runs are expected to queue still (`remaining`). None with `warm = 0`.
pub fn target(warm: u32, active: bool, remaining: usize) -> usize {
    if warm == 0 {
        return 0;
    }
    let base = if active {
        usize::try_from(warm).unwrap_or(usize::MAX)
    } else {
        0
    };
    base.saturating_add(remaining)
}

#[cfg(test)]
mod tests;
