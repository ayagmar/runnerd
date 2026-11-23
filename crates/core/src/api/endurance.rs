//! How long the drive under Docker's root lasts at the rate it writes, against what its maker rates it to write (ADR
//! 0157), as clients read it in the disk-writes section of `GET /v1/insights`, and every word they show of it. Sizes
//! are bytes and are worded in the decimal units drive makers rate and SMART counts in ([`DriveBytes`]), never in
//! [`ByteSize`](crate::config::ByteSize)'s binary ones, and each line says which it uses. Times are seconds since the
//! Unix epoch, spans seconds.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::units::{short_date, utc_time};
use super::{LabelledFigure, Tone, duration};
use crate::config::DriveBytes;

/// A day, in seconds.
const DAY: u64 = 86_400;

/// The drive's endurance: from `[host.drive]`, what its maker rates it to write and what its SMART log last said it
/// had written, and from the windows runnerd read of that disk, what it wrote since and at what rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endurance {
    /// The disk: `nvme3n1`.
    pub device: String,
    /// What its maker rates it to write, its TBW, in bytes.
    pub rated: u64,
    /// What it has written; `None` when the config gives no SMART count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written: Option<WrittenSince>,
    /// Its rate over the last 7 days; `None` when runnerd read none of its windows then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub week: Option<WriteRate>,
    /// Its rate over the last 24 hours; `None` when runnerd read none of its windows then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<WriteRate>,
    /// Days left at the week's rate under which it warns.
    pub warn_days: u32,
}

/// What the drive has written: its SMART log's count as the config gives it, and what runnerd read it write since.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrittenSince {
    /// What its SMART log said it had written, in bytes.
    pub counted: u64,
    /// When.
    pub at: u64,
    /// What runnerd read it write since, over the windows that started after `at`, in bytes.
    pub read: u64,
    /// The seconds those windows span.
    pub watched: u64,
    /// The seconds from `at` to the insights' end: what runnerd did not read of them is not in `read`.
    pub since: u64,
}

/// What the drive wrote over the windows runnerd read in a stretch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRate {
    /// Bytes.
    pub bytes: u64,
    /// The seconds the windows span.
    pub watched: u64,
}

impl WriteRate {
    /// What a day at this rate writes, in bytes.
    pub fn per_day(&self) -> u64 {
        let day = u128::from(self.bytes) * u128::from(DAY) / u128::from(self.watched.max(1));
        u64::try_from(day).unwrap_or(u64::MAX)
    }
}

impl WrittenSince {
    /// The count and what runnerd read since together: a floor when runnerd did not read every second since.
    pub fn total(&self) -> u64 {
        self.counted.saturating_add(self.read)
    }
}

/// Which stretch a rate is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateSpan {
    /// The last 7 days.
    Week,
    /// The last 24 hours.
    Day,
}

impl RateSpan {
    /// `the last 7 days'`, `the last 24 hours'`.
    fn label(self) -> &'static str {
        match self {
            RateSpan::Week => "the last 7 days'",
            RateSpan::Day => "the last 24 hours'",
        }
    }

    /// `the last 7 days`, `the last 24 hours`.
    fn within(self) -> &'static str {
        match self {
            RateSpan::Week => "the last 7 days",
            RateSpan::Day => "the last 24 hours",
        }
    }
}

impl Endurance {
    /// The section's title.
    pub const TITLE: &'static str = "Drive endurance";

    /// The rate over `span`.
    pub fn rate(&self, span: RateSpan) -> Option<WriteRate> {
        match span {
            RateSpan::Week => self.week,
            RateSpan::Day => self.day,
        }
    }

    /// What it may still write before its rating, in bytes; `None` without a written count.
    pub fn left(&self) -> Option<u64> {
        Some(self.rated.saturating_sub(self.written?.total()))
    }

    /// Days until it has written its rating at the rate over `span`, rounded down; `None` without a written count or
    /// a rate, or at a rate of nothing a day.
    pub fn days_left(&self, span: RateSpan) -> Option<u64> {
        let per_day = self.rate(span)?.per_day();
        self.left()?.checked_div(per_day)
    }

    /// Whether it warns: it has written its rating, or the week's rate reaches it within `warn_days`.
    pub fn warns(&self) -> bool {
        self.left() == Some(0)
            || self
                .days_left(RateSpan::Week)
                .is_some_and(|days| days < u64::from(self.warn_days))
    }

    /// What it means: a warning when it [`warns`](Endurance::warns).
    pub fn tone(&self) -> Tone {
        if self.warns() {
            Tone::Warn
        } else {
            Tone::Neutral
        }
    }

    /// It in lines: the rating and its unit; what the drive has written and where each part comes from, or how to
    /// give the count; and each rate with the days it leaves.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{} is rated to write {} (decimal terabytes, as drive makers and SMART count; runnerd's other sizes are \
             binary, 1T = 1.1TB)",
            self.device,
            DriveBytes(self.rated)
        )];
        match self.written {
            Some(written) => {
                let mut line = format!(
                    "It has written at least {}: {} its SMART log said on {} {}, per [host.drive], and {} runnerd \
                     read since, over {} of the {} since",
                    DriveBytes(written.total()),
                    DriveBytes(written.counted),
                    short_date(written.at),
                    utc_time(written.at),
                    DriveBytes(written.read),
                    duration(written.watched),
                    duration(written.since)
                );
                if written.watched < written.since {
                    line.push_str("; what it wrote while runnerd did not read it is not counted");
                }
                if written.total() >= self.rated {
                    let _ = write!(
                        line,
                        "; that is past its rating by {}",
                        DriveBytes(written.total() - self.rated)
                    );
                }
                lines.push(line);
            }
            None => lines.push(
                "What it has written is unknown: set [host.drive] written to smartctl's Data Units Written for the \
                 days left"
                    .to_owned(),
            ),
        }
        for span in [RateSpan::Week, RateSpan::Day] {
            let Some(rate) = self.rate(span) else {
                lines.push(format!(
                    "runnerd read none of {}'s writes in {}",
                    self.device,
                    span.within()
                ));
                continue;
            };
            let mut line = format!(
                "At {} rate, {} a day over the {} runnerd read it",
                span.label(),
                DriveBytes(rate.per_day()),
                duration(rate.watched)
            );
            match (self.left(), self.days_left(span)) {
                (Some(0) | None, _) => {}
                (Some(left), Some(days)) => {
                    let _ = write!(
                        line,
                        ": the {} left lasts {days} {}",
                        DriveBytes(left),
                        if days == 1 { "day" } else { "days" }
                    );
                }
                (Some(left), None) => {
                    let _ = write!(
                        line,
                        ": the {} left lasts while nothing is written",
                        DriveBytes(left)
                    );
                }
            }
            lines.push(line);
        }
        lines
    }

    /// Its figures, in the order they show: the rating, what is left of it and the days at each rate, when known.
    pub fn figures(&self) -> Vec<LabelledFigure> {
        let mut figures = vec![LabelledFigure::new(
            "Rated to write",
            DriveBytes(self.rated).to_string(),
        )];
        if let Some(left) = self.left() {
            figures.push(LabelledFigure::new(
                "Left of it",
                DriveBytes(left).to_string(),
            ));
        }
        for (span, label) in [
            (RateSpan::Week, "Days left at 7 days' rate"),
            (RateSpan::Day, "Days left at 24 hours' rate"),
        ] {
            if let Some(days) = self.days_left(span) {
                figures.push(LabelledFigure::new(label, days.to_string()));
            }
        }
        figures
    }

    /// What its warning insight says: `nvme3n1 reaches its rated 600TB written in 113 days at the last 7 days' rate
    /// of 4.6TB a day`, or that it is past it.
    pub fn sentence(&self) -> String {
        let rated = DriveBytes(self.rated);
        match (self.left(), self.days_left(RateSpan::Week), self.week) {
            (Some(0), _, _) => format!(
                "{} has written its rated {rated} (decimal terabytes, as SMART counts): its maker no longer \
                 promises it lasts",
                self.device
            ),
            (_, Some(days), Some(rate)) => format!(
                "{} reaches its rated {rated} written (decimal terabytes, as SMART counts) in {days} {} at the last \
                 7 days' rate of {} a day",
                self.device,
                if days == 1 { "day" } else { "days" },
                DriveBytes(rate.per_day())
            ),
            _ => format!("{} is rated to write {rated}", self.device),
        }
    }
}
