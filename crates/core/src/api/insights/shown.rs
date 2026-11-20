//! What insights and the digest mean as every client shows them: each insight's labels and tone, how many warn,
//! the window they come from, what the page says without any, and the digest's lines. Plain derivations of
//! [`Insights`] and [`Digest`]; the dashboard, `runnerctl`, the TUI and notifications only lay them out.

use super::{
    Digest, HostHazard, HostSpan, Insight, InsightKind, Insights, LongWait, ProfileQueue,
    RepositoryWaits, Severity, WaitSpread,
};
use crate::api::LabelledFigure;
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

/// The number an insight is about, short, for a figure beside its sentence: `6m 10s` over `from 4m 40s`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsightFigure {
    /// The number: `6m 10s`, `2 of 31`, `8.1G`, `4%`.
    pub value: String,
    /// What it is, short and lower case: `from 4m 40s`, `runs passed on a re-run`.
    pub label: String,
}

impl Insight {
    /// The number it is about, with what it is; `None` for a kind from a later release. Its sentence says the rest.
    pub fn figure(&self) -> Option<InsightFigure> {
        let figure = |value: String, label: String| Some(InsightFigure { value, label });
        let of = |part: u32, whole: u32| format!("{part} of {whole}");
        match &self.kind {
            InsightKind::Slower { before, now, .. } => {
                figure(duration(*now), format!("from {}", duration(*before)))
            }
            InsightKind::Flaky { flaky, total } => {
                figure(of(*flaky, *total), "runs passed on a re-run".to_owned())
            }
            InsightKind::NearCap {
                profile, peak, cap, ..
            } => figure(
                ByteSize(*peak).to_string(),
                format!("p95 peak, of {profile}'s {}", ByteSize(*cap)),
            ),
            InsightKind::ColdCache { hits, lookups, .. } => {
                let percent = (u128::from(*hits) * 100 + u128::from(*lookups) / 2)
                    / u128::from((*lookups).max(1));
                figure(
                    format!("{}%", percent.min(100)),
                    format!("of {lookups} lookups hit"),
                )
            }
            InsightKind::Throttled { throttled, total } => {
                figure(of(*throttled, *total), "runs throttled".to_owned())
            }
            InsightKind::Waits { before, now } => figure(
                duration(*now),
                format!("p95 wait, from {}", duration(*before)),
            ),
            InsightKind::CpuHeld {
                profile,
                cpus,
                held,
                total,
                ..
            } => figure(
                of(*held, *total),
                format!("runs held back by {profile}'s {cpus} CPUs"),
            ),
            InsightKind::HostHeld { jobs, waited } => figure(
                duration(*waited),
                if *jobs == 1 {
                    "1 job waits for the host's memory".to_owned()
                } else {
                    format!("longest of {jobs} waits for the host's memory")
                },
            ),
            InsightKind::HostLow { lowest, .. } => figure(
                ByteSize(*lowest).to_string(),
                "least memory available".to_owned(),
            ),
            InsightKind::Unknown => None,
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

    /// What the insights page says over its list: how many insights it lists, how many of them warn, and the window
    /// they come from.
    pub fn heading(&self) -> InsightsHeading {
        let count = self.insights.len();
        let figure = match count {
            0 => "no insights".to_owned(),
            1 => "1 insight".to_owned(),
            count => format!("{count} insights"),
        };
        let note = match self.warning_count() {
            0 => format!(" {}", self.window_line()),
            1 => format!(", 1 warning, {}", self.window_line()),
            warnings => format!(", {warnings} warnings, {}", self.window_line()),
        };
        InsightsHeading { figure, note }
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

/// What the insights page says over its list, as [`Insights::heading`] words it: `5 insights, 4 warnings, from the
/// last 14 days`, `1 insight from the last 14 days`, `no insights from the last 14 days`. A client may set the figure
/// apart; together, as its `Display` writes them, they read as one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsightsHeading {
    /// How many it lists: `5 insights`, `1 insight`, `no insights`.
    pub figure: String,
    /// The rest, starting with what joins it to the figure: `, 4 warnings, from the last 14 days`, or ` from the last
    /// 14 days` when none warns.
    pub note: String,
}

impl std::fmt::Display for InsightsHeading {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}{}", self.figure, self.note)
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
    /// into; [host] pooled_count = false makes the budgets count every reservation again.`
    pub fn host_low_sentence(day: u64, seconds: u64, lowest: u64, line: u64) -> String {
        format!(
            "On {} the host had under {} available for {} while a host credit stood, down to {}. The credit lets \
             runners start on memory the running ones may still grow into; [host] pooled_count = false makes the \
             budgets count every reservation again.",
            short_date(day),
            ByteSize(line),
            duration(seconds),
            ByteSize(lowest)
        )
    }
}

impl HostHazard {
    /// What the host's memory while CI ran is called where it shows, as a heading or before its line.
    pub const TITLE: &'static str = "Host memory while CI ran";

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
            .map(|host| format!("{}: {}.", HostHazard::TITLE, host.line()))
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

/// One of a repository's wait figures: its label, such as `Warm median`, and its value, `–` without jobs to take it
/// over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitFigure {
    /// `Jobs`, `Median`, `p95`, `Warm median`, `Cold median` or `p95 the week before`.
    pub label: &'static str,
    /// The count of jobs, or a wait such as `4m 10s`.
    pub value: String,
}

impl RepositoryWaits {
    /// What a repository's waits are called where they show.
    pub const TITLE: &'static str = "Waits this week";

    /// Its figures, in the order they show: the week's jobs, their median and p95 wait, the warm and the cold
    /// median, and the week before's p95.
    pub fn figures(&self) -> Vec<WaitFigure> {
        let wait = |spread: &WaitSpread, value: Option<u64>| {
            spread
                .reading(value)
                .map_or_else(|| "–".to_owned(), duration)
        };
        let now = &self.now;
        let before = &self.before.all;
        [
            (crate::api::JOBS_HEADING, now.all.jobs.to_string()),
            ("Median", wait(&now.all, now.all.p50)),
            ("p95", wait(&now.all, now.all.p95)),
            ("Warm median", wait(&now.warm, now.warm.p50)),
            ("Cold median", wait(&now.cold, now.cold.p50)),
            ("p95 the week before", wait(before, before.p95)),
        ]
        .into_iter()
        .map(|(label, value)| WaitFigure { label, value })
        .collect()
    }
}

impl ProfileQueue {
    /// What the split of the profiles' queue time is called where it shows.
    pub const TITLE: &'static str = "Why jobs queued";
    /// The label of the wait before runnerd first listed a job.
    pub const BEFORE_LISTED: &'static str = "Before runnerd listed it";
    /// The label of the wait no plan held a job for.
    pub const UNCOVERED: &'static str = "No plan covered";

    /// It in a line: `jvm: 42 jobs waited 12h 30m in all`, then `; 3 jobs from before runnerd counted why are left
    /// out` when some are; `jvm: 3 jobs ended from before runnerd counted why jobs wait` when none is counted.
    pub fn line(&self) -> String {
        let jobs = |count: u32| {
            if count == 1 {
                "1 job".to_owned()
            } else {
                format!("{count} jobs")
            }
        };
        if self.jobs == 0 {
            return format!(
                "{}: {} ended from before runnerd counted why jobs wait",
                self.profile,
                jobs(self.unrecorded)
            );
        }
        let mut line = format!(
            "{}: {} waited {} in all",
            self.profile,
            jobs(self.jobs),
            duration(self.waited)
        );
        if self.unrecorded > 0 {
            let _ = write!(
                line,
                "; {} from before runnerd counted why {} left out",
                jobs(self.unrecorded),
                if self.unrecorded == 1 { "is" } else { "are" }
            );
        }
        line
    }

    /// What held the jobs, longest first, each with its share of their waits: `Host memory` `6h 10m` `49%`; then
    /// the wait before runnerd listed them and the wait no plan covered, when there was some. Empty when no job was
    /// counted.
    pub fn figures(&self) -> Vec<LabelledFigure> {
        let share = |seconds: u64| {
            let percent = (u128::from(seconds) * 100 + u128::from(self.waited) / 2)
                / u128::from(self.waited.max(1));
            format!("{}%", percent.min(100))
        };
        let figure = |label: &str, seconds: u64| {
            LabelledFigure::new(label, duration(seconds)).of(share(seconds))
        };
        let mut figures: Vec<LabelledFigure> = self
            .causes
            .iter()
            .map(|cause| figure(cause.cause.label(), cause.waited))
            .collect();
        for (label, seconds) in [
            (Self::BEFORE_LISTED, self.before_listed),
            (Self::UNCOVERED, self.uncovered),
        ] {
            if seconds > 0 {
                figures.push(figure(label, seconds));
            }
        }
        figures
    }
}

#[cfg(test)]
mod tests;
