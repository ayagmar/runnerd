//! What insights and the digest mean as every client shows them: each insight's labels and tone, how many warn,
//! the window they come from, what the page says without any, and the digest's lines. Plain derivations of
//! [`Insights`] and [`Digest`]; the dashboard, `runnerctl`, the TUI and notifications only lay them out.

use super::{
    Digest, HostHazard, HostSpan, Insight, InsightKind, Insights, LongWait, Severity, WaitSpread,
};
use crate::api::units::short_date;
use crate::api::{HostMemory, Tone, duration};
use crate::config::ByteSize;
use std::fmt::Write as _;

impl InsightKind {
    /// Its name as a label: `Slower`, `Flaky`, `Near its cap`, `Cold cache`, `Memory pressure`, `CPU cap`,
    /// `Host memory`, `Host low`, `Waits`, or `Insight` for a kind from a later release.
    pub fn label(&self) -> &'static str {
        match self {
            InsightKind::Slower { .. } => "Slower",
            InsightKind::Flaky { .. } => "Flaky",
            InsightKind::NearCap { .. } => "Near its cap",
            InsightKind::ColdCache { .. } => "Cold cache",
            InsightKind::Throttled { .. } => "Memory pressure",
            InsightKind::CpuHeld { .. } => "CPU cap",
            InsightKind::HostHeld { .. } => "Host memory",
            InsightKind::HostLow { .. } => "Host low",
            InsightKind::Waits { .. } => "Waits",
            InsightKind::Unknown => "Insight",
        }
    }

    /// Its name as one word, the API's `kind`: `slower`, `flaky`, `near-cap`, `cold-cache`, `throttled`,
    /// `cpu-held`, `host-held`, `host-low` or `waits`; `unknown` for a kind from a later release.
    pub fn word(&self) -> &'static str {
        match self {
            InsightKind::Slower { .. } => "slower",
            InsightKind::Flaky { .. } => "flaky",
            InsightKind::NearCap { .. } => "near-cap",
            InsightKind::ColdCache { .. } => "cold-cache",
            InsightKind::Throttled { .. } => "throttled",
            InsightKind::CpuHeld { .. } => "cpu-held",
            InsightKind::HostHeld { .. } => "host-held",
            InsightKind::HostLow { .. } => "host-low",
            InsightKind::Waits { .. } => "waits",
            InsightKind::Unknown => "unknown",
        }
    }
}

impl Insight {
    /// What a [`HostHeld`](InsightKind::HostHeld) insight says: `3 jobs wait for the host's memory, the longest for
    /// 42m 10s (halyard/api: CI / build); programs outside CI hold 30G, most java 9.3G and claude 5.1G. Closing programs
    /// frees only part of that.` `longest` names the job that waited longest; `memory` is the
    /// reading that names who holds the memory. The part the programs named leave is the kernel, compressed swap and
    /// the many small programs, so the sentence never says closing them frees the whole figure.
    pub fn host_held_sentence(
        jobs: u32,
        waited: u64,
        longest: &str,
        memory: &HostMemory,
    ) -> String {
        let who = if jobs == 1 {
            "1 job waits".to_owned()
        } else {
            format!("{jobs} jobs wait")
        };
        format!(
            "{who} for the host's memory, the longest for {} ({longest}); {}. Closing programs frees only part of \
             that.",
            duration(waited),
            memory.outside_line()
        )
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

    /// How many insights warn, worded for the overview: `3 insights`; `None` when none does.
    pub fn warning_line(&self) -> Option<String> {
        match self.warning_count() {
            0 => None,
            1 => Some("1 insight".to_owned()),
            count => Some(format!("{count} insights")),
        }
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

impl Insight {
    /// What a [`HostLow`](InsightKind::HostLow) insight says: `On Nov 6 the host had under 6.2G available for 34s while
    /// a host credit stood, down to 4.1G. The credit lets runners start on memory the running ones may still grow
    /// into; [host] pooled_host = false makes the host's limit count every reservation again.`
    pub fn host_low_sentence(day: u64, seconds: u64, lowest: u64, line: u64) -> String {
        format!(
            "On {} the host had under {} available for {} while a host credit stood, down to {}. The credit lets \
             runners start on memory the running ones may still grow into; [host] pooled_host = false makes the \
             host's limit count every reservation again.",
            short_date(day),
            ByteSize(line),
            duration(seconds),
            ByteSize(lowest)
        )
    }
}

impl HostHazard {
    /// The window in a line: `CI ran 9h 12m, 6h 10m of it on a host credit; under 19.4G available for 1h 04m (41m on
    /// a credit); under 6.2G available for 34s (34s on a credit); lowest 4.1G (4.5G on a credit); swap free at least
    /// 8.2G`.
    /// Without a host credit in it, nothing is said of one.
    pub fn line(&self) -> String {
        let credited = self.credited > 0;
        let share = |seconds: u64| {
            if credited {
                format!(" ({} on a credit)", duration(seconds))
            } else {
                String::new()
            }
        };
        let under = |line: u64, span: HostSpan| match span.seconds {
            0 => format!("never under {} available", ByteSize(line)),
            seconds => format!(
                "under {} available for {}{}",
                ByteSize(line),
                duration(seconds),
                share(span.credited)
            ),
        };
        let mut line = format!("CI ran {}", duration(self.running));
        if credited {
            let _ = write!(line, ", {} of it on a host credit", duration(self.credited));
        }
        let _ = write!(
            line,
            "; {}; {}; lowest {}",
            under(self.low_line, self.low),
            under(self.critical_line, self.critical),
            ByteSize(self.lowest)
        );
        if let Some(lowest) = self.lowest_credited {
            let _ = write!(line, " ({} on a credit)", ByteSize(lowest));
        }
        if let Some(swap) = self.lowest_swap_free {
            let _ = write!(line, "; swap free at least {}", ByteSize(swap));
        }
        line
    }

    /// The window's UTC day and its line, as `runnerctl insights` and the dashboard list the days: `Nov 6: CI ran …`.
    pub fn dated_line(&self) -> String {
        format!("{}: {}", short_date(self.since), self.line())
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

    /// The host's memory over its day in a line, `Host memory while CI ran: CI ran …`, or nothing when CI did not
    /// run.
    pub fn host_line(&self) -> Option<String> {
        self.host
            .as_ref()
            .map(|host| format!("Host memory while CI ran: {}.", host.line()))
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
            duration(self.waited)
        )
    }
}

#[cfg(test)]
mod tests;
