//! What the job history's statistics mean as every client shows them: a window's jobs summed, by day or whole,
//! with their averages, success share and sampled peak. Plain derivations of [`Stats`]; the dashboard's history
//! page only draws them.

use super::{Stats, StatsRow};

/// Finished jobs summed over a time, every repository of the query together, from `Stats::daily`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSummary {
    /// Its start.
    pub start: u64,
    /// Jobs completed or lost.
    pub jobs: u64,
    /// Of those, concluded `success`.
    pub succeeded: u64,
    /// Of those, failed, timed out or lost.
    pub failed: u64,
    /// Of those, cancelled.
    pub cancelled: u64,
    /// Seconds from queued to started, summed.
    pub queue_seconds: u64,
    /// Seconds from started to finished, summed.
    pub run_seconds: u64,
    /// The highest sampled memory peak of a job; `None` when no job had a sample.
    pub memory_peak: Option<u64>,
}

impl StatsSummary {
    fn add(&mut self, row: &StatsRow) {
        self.jobs += row.jobs;
        self.succeeded += row.succeeded;
        self.failed += row.failed;
        self.cancelled += row.cancelled;
        self.queue_seconds += row.queue_seconds;
        self.run_seconds += row.run_seconds;
        self.memory_peak = self
            .memory_peak
            .max((row.memory_peak > 0).then_some(row.memory_peak));
    }

    /// `summaries` together, starting at the first's start.
    pub fn of(summaries: &[StatsSummary]) -> StatsSummary {
        summaries.iter().fold(
            StatsSummary {
                start: summaries.first().map_or(0, |summary| summary.start),
                ..StatsSummary::default()
            },
            |total, summary| StatsSummary {
                start: total.start,
                jobs: total.jobs + summary.jobs,
                succeeded: total.succeeded + summary.succeeded,
                failed: total.failed + summary.failed,
                cancelled: total.cancelled + summary.cancelled,
                queue_seconds: total.queue_seconds + summary.queue_seconds,
                run_seconds: total.run_seconds + summary.run_seconds,
                memory_peak: total.memory_peak.max(summary.memory_peak),
            },
        )
    }

    /// Jobs that finished otherwise: neither succeeded, failed nor cancelled, as a skipped one.
    pub fn other(&self) -> u64 {
        self.jobs
            .saturating_sub(self.succeeded + self.failed + self.cancelled)
    }

    /// The share of its jobs that succeeded, in whole percent rounded down; `None` without jobs.
    pub fn success_percent(&self) -> Option<u64> {
        (self.succeeded * 100).checked_div(self.jobs)
    }

    /// Its jobs' average run, in seconds; `None` without jobs.
    pub fn average_run(&self) -> Option<u64> {
        self.run_seconds.checked_div(self.jobs)
    }

    /// Its jobs' average wait from queued to started, in seconds; `None` without jobs.
    pub fn average_wait(&self) -> Option<u64> {
        self.queue_seconds.checked_div(self.jobs)
    }
}

impl Stats {
    /// `days` days from `first`, the start of a UTC day, each summing every row whose bucket starts on it: a day
    /// without jobs is there, empty. Rows of other buckets than days, or outside the days, are left out.
    pub fn daily(&self, first: u64, days: u64) -> Vec<StatsSummary> {
        let mut summaries: Vec<StatsSummary> = (0..days)
            .map(|day| StatsSummary {
                start: first + day * 86_400,
                ..StatsSummary::default()
            })
            .collect();
        for row in &self.rows {
            if let Some(summary) = summaries
                .iter_mut()
                .find(|summary| summary.start == row.start)
            {
                summary.add(row);
            }
        }
        summaries
    }
}

#[cfg(test)]
mod tests;
