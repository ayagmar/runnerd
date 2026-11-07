//! What the host's memory came to while CI ran, from the readings the budget's events stored (ADR 0108): for each UTC
//! day and over the digest's 24 hours, with the share that came while a host credit stood; and a warning for the
//! latest day it fell under a tenth of the host while one stood. Pure, as the rest of insights.

use super::{DAY, HostReading, SECOND};
use crate::api::{HostHazard, HostSpan, Insight, InsightKind, Severity, host_margin};

/// How long a reading speaks for at most, in ms: the daemon stores one every few seconds while it runs, so a longer
/// gap is time it did not run, of which nothing is known.
const READING_LASTS: u64 = 60 * SECOND;

/// How long under each line, in ms, and of that while a host credit stood.
#[derive(Default)]
struct Under {
    all: u64,
    credited: u64,
}

impl Under {
    fn add(&mut self, span: u64, credited: bool) {
        self.all += span;
        self.credited += if credited { span } else { 0 };
    }

    fn seconds(&self) -> HostSpan {
        HostSpan {
            seconds: self.all.div_ceil(SECOND),
            credited: self.credited.div_ceil(SECOND),
        }
    }
}

/// What the host's memory came to from `since` to `until` (ms) by `readings`, oldest first; `None` when no runner ran
/// with a reading. Each reading counts until the next, at most `READING_LASTS`, and only while a runner ran. The lines
/// are those of the host's memory at the last reading.
pub(super) fn hazard(readings: &[HostReading], since: u64, until: u64) -> Option<HostHazard> {
    let mut running = Under::default();
    let mut low = Under::default();
    let mut critical = Under::default();
    let mut lowest: Option<u64> = None;
    let mut lowest_credited: Option<u64> = None;
    let mut lowest_swap_free: Option<u64> = None;
    let mut lines = (0, 0);
    for (index, reading) in readings.iter().enumerate() {
        if reading.at >= until {
            break;
        }
        let next = readings.get(index + 1).map_or(until, |next| next.at);
        let start = reading.at.max(since);
        let end = next.min(until).min(reading.at + READING_LASTS);
        if !reading.running || end <= start {
            continue;
        }
        let span = end - start;
        let credited = reading.credit > 0;
        let low_line = host_margin(reading.total) + reading.total / 4;
        let critical_line = reading.total / 10;
        lines = (low_line, critical_line);
        running.add(span, credited);
        if reading.available < low_line {
            low.add(span, credited);
        }
        if reading.available < critical_line {
            critical.add(span, credited);
        }
        let least = |lowest: Option<u64>, value: u64| {
            Some(lowest.map_or(value, |lowest| lowest.min(value)))
        };
        lowest = least(lowest, reading.available);
        if credited {
            lowest_credited = least(lowest_credited, reading.available);
        }
        if let Some(free) = reading.swap_free {
            lowest_swap_free = least(lowest_swap_free, free);
        }
    }
    Some(HostHazard {
        since: since / SECOND,
        until: until / SECOND,
        running: running.seconds().seconds,
        credited: running.seconds().credited,
        low_line: lines.0,
        critical_line: lines.1,
        low: low.seconds(),
        critical: critical.seconds(),
        lowest: lowest?,
        lowest_credited,
        lowest_swap_free,
    })
}

/// What the host's memory came to on each UTC day from `since` to `until` (ms) that CI ran, newest first; the first
/// and the last day only in part.
pub(super) fn days(readings: &[HostReading], since: u64, until: u64) -> Vec<HostHazard> {
    let first = since / DAY * DAY;
    let mut days: Vec<HostHazard> = (first..until)
        .step_by(usize::try_from(DAY).unwrap_or(usize::MAX))
        .filter_map(|day| hazard(readings, day.max(since), (day + DAY).min(until)))
        .collect();
    days.reverse();
    days
}

/// The warning for the latest of `days` on which the host fell under a tenth of its memory while a host credit stood.
/// Its key is that day's, so a notification target tells it once a day at most.
pub(super) fn low(days: &[HostHazard]) -> Option<Insight> {
    let day = days.iter().find(|day| day.critical.credited > 0)?;
    let lowest = day.lowest_credited.unwrap_or(day.lowest);
    Some(Insight {
        key: format!("host-low:{}", day.since * SECOND / DAY),
        severity: Severity::Warning,
        repository: String::new(),
        workflow: None,
        job: None,
        sentence: Insight::host_low_sentence(
            day.since,
            day.critical.credited,
            lowest,
            day.critical_line,
        ),
        kind: InsightKind::HostLow {
            day: day.since,
            seconds: day.critical.credited,
            lowest,
        },
        runs: Vec::new(),
    })
}
