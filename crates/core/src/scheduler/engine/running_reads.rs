//! When the running jobs are next read from GitHub (ADR 0124): once every `poll_interval`, whatever the ticks' pace.
//! It owns that one due time. It must not decide what a read does with its answers, nor gate the reads a plan or a
//! job's end waits for (`follow_jobs`' details of an unnamed job, the closing reads, the uploads of an ended runner);
//! those stay with their callers.
//!
//! Nothing survives a restart: the first tick after one reads every running job, as before the pace. A reload keeps
//! the due time, and takes the new interval from the next tick.

use std::time::Duration;

use tokio::time::Instant;

/// The tick whose listing began the last read of the running jobs.
#[derive(Debug, Default)]
pub(super) struct RunningReads {
    read_at: Option<Instant>,
}

impl RunningReads {
    /// Whether a tick whose listing began at `anchor` reads the running jobs: the first tick, or one at least
    /// `interval` after the tick that read them last.
    pub(super) fn due(&self, anchor: Instant, interval: Duration) -> bool {
        self.read_at
            .is_none_or(|read_at| anchor.saturating_duration_since(read_at) >= interval)
    }

    /// Notes that the tick whose listing began at `anchor` read them.
    pub(super) fn read(&mut self, anchor: Instant) {
        self.read_at = Some(anchor);
    }

    /// As if the last read was `by` earlier.
    #[cfg(test)]
    pub(super) fn age(&mut self, by: Duration) {
        self.read_at = self.read_at.and_then(|read_at| read_at.checked_sub(by));
    }
}
