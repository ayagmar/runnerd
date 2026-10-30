//! What insights and the digest mean as every client shows them: each insight's labels and tone, how many warn,
//! the window they come from, what the page says without any, and the digest's lines. Plain derivations of
//! [`Insights`] and [`Digest`]; the dashboard, `runnerctl`, the TUI and notifications only lay them out.

use super::{Digest, Insight, InsightKind, Insights, LongWait, Severity, WaitSpread};
use crate::api::Tone;

impl InsightKind {
    /// Its name as a label: `Slower`, `Flaky`, `Near its cap`, `Cold cache`, `Memory pressure`, `CPU cap`,
    /// `Waits`, or `Insight` for a kind from a later release.
    pub fn label(&self) -> &'static str {
        match self {
            InsightKind::Slower { .. } => "Slower",
            InsightKind::Flaky { .. } => "Flaky",
            InsightKind::NearCap { .. } => "Near its cap",
            InsightKind::ColdCache { .. } => "Cold cache",
            InsightKind::Throttled { .. } => "Memory pressure",
            InsightKind::CpuHeld { .. } => "CPU cap",
            InsightKind::Waits { .. } => "Waits",
            InsightKind::Unknown => "Insight",
        }
    }

    /// Its name as one word, the API's `kind`: `slower`, `flaky`, `near-cap`, `cold-cache`, `throttled`,
    /// `cpu-held` or `waits`; `unknown` for a kind from a later release.
    pub fn word(&self) -> &'static str {
        match self {
            InsightKind::Slower { .. } => "slower",
            InsightKind::Flaky { .. } => "flaky",
            InsightKind::NearCap { .. } => "near-cap",
            InsightKind::ColdCache { .. } => "cold-cache",
            InsightKind::Throttled { .. } => "throttled",
            InsightKind::CpuHeld { .. } => "cpu-held",
            InsightKind::Waits { .. } => "waits",
            InsightKind::Unknown => "unknown",
        }
    }
}

impl Severity {
    /// `warning` or `notice`.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Notice => "notice",
        }
    }
}

impl Insight {
    /// What it means for the reader, by its severity alone, whatever its kind: a warning is worth a look, a notice
    /// a plain fact.
    pub fn tone(&self) -> Tone {
        match self.severity {
            Severity::Warning => Tone::Warn,
            Severity::Notice => Tone::Neutral,
        }
    }
}

impl Insights {
    /// How many insights warn.
    pub fn warning_count(&self) -> usize {
        self.insights
            .iter()
            .filter(|insight| insight.severity == Severity::Warning)
            .count()
    }

    /// The whole days its window covers.
    pub fn days(&self) -> u64 {
        self.until.saturating_sub(self.since) / 86_400
    }

    /// Where its insights come from: `from the last 30 days`.
    pub fn window_line(&self) -> String {
        format!("from the last {} days", self.days())
    }

    /// What it says without any insight: `nothing stands out from the last 30 days`.
    pub fn empty_line(&self) -> String {
        format!("nothing stands out {}", self.window_line())
    }
}

impl WaitSpread {
    /// `value`, one of its percentiles, when it has jobs to take it over; `None` to show none.
    pub fn reading(&self, value: Option<u64>) -> Option<u64> {
        value.filter(|_| self.jobs > 0)
    }
}

impl Digest {
    /// Its day in a line: `12 jobs finished, 2 failed`, then `, 1 new insight` when some are new.
    pub fn summary_line(&self) -> String {
        let jobs = if self.finished == 1 { "job" } else { "jobs" };
        let line = format!("{} {jobs} finished, {} failed", self.finished, self.failed);
        match self.new.len() {
            0 => line,
            1 => format!("{line}, 1 new insight"),
            new => format!("{line}, {new} new insights"),
        }
    }
}

impl LongWait {
    /// It in a line: `halyard/api CI / test waited 1m 30s`, or `halyard/api job 7 waited 1m 30s` until the daemon
    /// knows the job's name.
    pub fn line(&self) -> String {
        format!(
            "{} {} waited {}",
            self.repository,
            crate::api::job_title(self.workflow.as_deref(), &self.name, Some(self.job)),
            crate::api::duration(self.waited)
        )
    }
}

#[cfg(test)]
mod tests;
