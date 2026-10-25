//! What the disk runnerd keeps means as every client shows it: what each cache volume holds and what its limit does,
//! what the build cache did for a job and over a week, how full the records are and what became of a job's record,
//! and how much of a job's log is stored. Plain derivations of the API's types; clients only lay them out.

use super::{
    BuildCacheCounts, BuildCacheStats, CacheKind, CacheVolume, LogSummary, RecordLimits,
    RecordNote, RecordRemoval, Records, Tone, period,
};
use crate::config::ByteSize;

/// The share of `[records] max_total` past which the records are near it, in percent: new ones then remove the
/// oldest to fit.
pub const RECORDS_NEAR_FULL: u64 = 80;

/// Whether records taking `bytes` are near `max_total` (`RECORDS_NEAR_FULL`).
pub fn records_near_full(bytes: u64, max_total: u64) -> bool {
    u128::from(bytes) * 100 >= u128::from(max_total) * u128::from(RECORDS_NEAR_FULL)
}

impl CacheKind {
    /// What it holds, as every client names it: `jvm snapshot`, `jvm slot 2 layer`, `pnpm store, shared`, `Maven
    /// repository, shared`, `Docker Hub mirror` or `build cache, shared`.
    pub fn description(&self) -> String {
        match self {
            CacheKind::Snapshot { profile } => format!("{profile} snapshot"),
            CacheKind::Slot { profile, slot } => format!("{profile} slot {slot} layer"),
            CacheKind::Pnpm => "pnpm store, shared".to_owned(),
            CacheKind::Maven => "Maven repository, shared".to_owned(),
            CacheKind::Mirror => "Docker Hub mirror".to_owned(),
            CacheKind::Build => "build cache, shared".to_owned(),
        }
    }

    /// The profile it belongs to; `None` for a shared one.
    pub fn profile(&self) -> Option<&str> {
        match self {
            CacheKind::Snapshot { profile } | CacheKind::Slot { profile, .. } => Some(profile),
            CacheKind::Pnpm | CacheKind::Maven | CacheKind::Mirror | CacheKind::Build => None,
        }
    }
}

/// A cache volume against its limit, from `CacheVolume::limit_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheLimit {
    /// In bytes.
    pub limit: u64,
    /// Whether the volume is past it, so runnerd acts on it.
    pub over: bool,
    /// What runnerd does at it: `emptied above 5G`, `trimmed to 20G`, or for a snapshot `started over at its next
    /// reseed above 30G`.
    pub label: String,
}

impl CacheLimit {
    /// What its size means: past the limit is worth a look.
    pub fn tone(&self) -> Tone {
        if self.over { Tone::Warn } else { Tone::Neutral }
    }
}

impl CacheVolume {
    /// Its size against its limit, when it has one.
    pub fn limit_status(&self) -> Option<CacheLimit> {
        let limit = self.limit?;
        let size = ByteSize(limit);
        let label = match self.kind {
            CacheKind::Build => format!("trimmed to {size}"),
            CacheKind::Snapshot { .. } => format!("started over at its next reseed above {size}"),
            CacheKind::Slot { .. } | CacheKind::Pnpm | CacheKind::Maven | CacheKind::Mirror => {
                format!("emptied above {size}")
            }
        };
        Some(CacheLimit {
            limit,
            over: self.bytes > limit,
            label,
        })
    }
}

/// What the build cache did for one job, from `BuildCacheCounts::activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildCacheActivity {
    /// The counts.
    pub counts: BuildCacheCounts,
}

impl BuildCacheCounts {
    /// What it did, for a job that used it at all: looked an entry up, stored one, or was refused one. `None` for
    /// a job that never used it.
    pub fn activity(&self) -> Option<BuildCacheActivity> {
        (self.lookups() > 0 || self.written_bytes > 0 || self.refused > 0)
            .then_some(BuildCacheActivity { counts: *self })
    }
}

impl BuildCacheActivity {
    /// Its lookups: `3 of 4 lookups hit (75%)`, or `no lookups`.
    pub fn lookups_line(&self) -> String {
        let counts = &self.counts;
        match counts.hit_percent() {
            Some(percent) => format!(
                "{} of {} lookups hit ({percent}%)",
                counts.hits,
                counts.lookups()
            ),
            None => "no lookups".to_owned(),
        }
    }

    /// What it served: `1.5G served`.
    pub fn served_line(&self) -> String {
        format!("{} served", ByteSize(self.counts.read_bytes))
    }

    /// What it stored: `2G stored`, or `read only` for a job that may not write.
    pub fn stored_line(&self) -> String {
        if self.counts.writer {
            format!("{} stored", ByteSize(self.counts.written_bytes))
        } else {
            "read only".to_owned()
        }
    }

    /// The uploads it refused, when it refused any: `3 uploads refused`.
    pub fn refused_line(&self) -> Option<String> {
        (self.counts.refused > 0).then(|| format!("{} uploads refused", self.counts.refused))
    }

    /// All of it in a line: `3 of 4 lookups hit (75%), 1.5G served, 2G stored, 3 uploads refused`.
    pub fn line(&self) -> String {
        let mut parts = vec![self.lookups_line(), self.served_line(), self.stored_line()];
        parts.extend(self.refused_line());
        parts.join(", ")
    }
}

impl BuildCacheStats {
    /// Its entries and the last 7 days' jobs: `120 entries; 75% of 400 lookups hit in 7 days, 3G served, 1G
    /// stored, 2 uploads refused`, or `120 entries; no lookups in 7 days`.
    pub fn week_line(&self) -> String {
        let week = &self.week;
        let served = match week.hit_percent() {
            Some(percent) => {
                let mut served = format!(
                    "{percent}% of {} lookups hit in 7 days, {} served, {} stored",
                    week.lookups(),
                    ByteSize(week.read_bytes),
                    ByteSize(week.written_bytes)
                );
                if week.refused > 0 {
                    served = format!("{served}, {} uploads refused", week.refused);
                }
                served
            }
            None => "no lookups in 7 days".to_owned(),
        };
        format!("{} entries; {served}", self.entries)
    }
}

impl RecordLimits {
    /// Whether records taking `bytes` are near `max_total` (`records_near_full`).
    pub fn near_full(&self, bytes: u64) -> bool {
        records_near_full(bytes, self.max_total)
    }
}

impl Records {
    /// How many there are and how much they take of their limits: `3 of 20 records, 1.2G of 10G, each kept 7
    /// days`, and when near `max_total`, `; near max_total, so new ones remove the oldest`.
    pub fn summary(&self) -> String {
        let limits = &self.limits;
        // `[records] max_age` is in days more often than not.
        let age = match limits.max_age / 86_400 {
            1 if limits.max_age == 86_400 => "1 day".to_owned(),
            days if days > 1 && limits.max_age.is_multiple_of(86_400) => format!("{days} days"),
            _ => period(limits.max_age),
        };
        let line = format!(
            "{} of {} records, {} of {}, each kept {age}",
            self.records.len(),
            limits.max_count,
            ByteSize(self.bytes()),
            ByteSize(limits.max_total),
        );
        if limits.near_full(self.bytes()) {
            format!("{line}; near max_total, so new ones remove the oldest")
        } else {
            line
        }
    }

    /// What the summary means: near `max_total` is worth a look.
    pub fn summary_tone(&self) -> Tone {
        if self.limits.near_full(self.bytes()) {
            Tone::Warn
        } else {
            Tone::Neutral
        }
    }

    /// Why there are none: `none kept: a profile with record_failures = true keeps each failed job's runner as a
    /// record, to open a shell where it failed`.
    pub fn empty_reason() -> &'static str {
        "none kept: a profile with record_failures = true keeps each failed job's runner as a record, to open a \
         shell where it failed"
    }

    /// Whether a shell is open in `job`'s record.
    pub fn shell_open(&self, job: u64) -> bool {
        self.shells.contains(&job)
    }
}

/// What became of a failed job's record, from `RecordNote::state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordState<'note> {
    /// None was made.
    Skipped {
        /// Why, such as too little free disk.
        why: &'note str,
    },
    /// It was made, then removed.
    Removed {
        /// When.
        at: u64,
        /// Why, when known.
        by: Option<RecordRemoval>,
    },
    /// It is kept: a shell can open in it.
    Kept {
        /// Its size when made, when known.
        bytes: Option<u64>,
    },
}

impl RecordNote {
    /// What became of the record: not made, made and removed, or kept, in that order of precedence.
    pub fn state(&self) -> RecordState<'_> {
        match (&self.skipped, self.removed_at) {
            (Some(why), _) => RecordState::Skipped { why },
            (None, Some(at)) => RecordState::Removed {
                at,
                by: self.removed_by,
            },
            (None, None) => RecordState::Kept { bytes: self.bytes },
        }
    }

    /// The command that opens a shell in `job`'s record: `runnerctl shell 42`.
    pub fn shell_command(job: u64) -> String {
        format!("runnerctl shell {job}")
    }
}

impl RecordState<'_> {
    /// Its words, before when it was removed for a removed one: `not kept: too little free disk`, `removed past
    /// max_age`, or `record kept, 2G`.
    pub fn line(&self) -> String {
        match self {
            RecordState::Skipped { why } => format!("not kept: {why}"),
            RecordState::Removed { by, .. } => {
                by.map_or_else(|| "removed".to_owned(), |by| by.to_string())
            }
            RecordState::Kept { bytes: Some(bytes) } => {
                format!("record kept, {}", ByteSize(*bytes))
            }
            RecordState::Kept { bytes: None } => "record kept".to_owned(),
        }
    }
}

impl LogSummary {
    /// How much of the log is stored: `120 lines, 4M`, and `, cut at history.log_max` once later lines were
    /// dropped.
    pub fn description(&self) -> String {
        let line = format!("{} lines, {}", self.lines, ByteSize(self.bytes));
        if self.truncated {
            format!("{line}, cut at history.log_max")
        } else {
            line
        }
    }
}

#[cfg(test)]
mod tests;
