//! Which of a log's lines a client reads to show it (ADR 0178): its last lines, or, for a line a reader cites
//! before them (a search hit, a failure's last lines), a bounded window around that line, started at the outermost
//! group that holds it so its folds read whole. Also what a client says when it shows a window instead of the
//! log's end, and why a cited line is not among the lines read. Pure: it reads only the outline, never the lines.

use super::{LineRange, LogEvidence, LogOutline};

/// The lines of a log a client reads, at most a size it picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogWindow {
    /// The log's last lines: what a client shows when no line is cited, or the cited line is among them or past
    /// the end.
    Tail(LineRange),
    /// Lines around `line`, a cited line before the last ones: not the log's end.
    Around {
        /// The cited line, 0-based.
        line: u64,
        /// The lines to read; they hold `line`.
        range: LineRange,
    },
}

impl LogWindow {
    /// The lines to read.
    pub fn range(&self) -> LineRange {
        match *self {
            LogWindow::Tail(range) | LogWindow::Around { range, .. } => range,
        }
    }

    /// What a client says of the lines it shows, `shown` of them read, of a log of `lines`: the tail's
    /// [`LogOutline::tail_words`], or `lines 1 to 5000 of 9200, around line 15, not the log's end`.
    pub fn words(&self, shown: usize, lines: u64) -> Option<String> {
        match *self {
            LogWindow::Tail(range) => LogOutline::tail_words(range.from, shown),
            LogWindow::Around { line, range } => {
                let last = range.from + shown as u64;
                Some(format!(
                    "lines {} to {last} of {lines}, around line {}, not the log's end",
                    range.from + 1,
                    line + 1
                ))
            }
        }
    }
}

impl LogOutline {
    /// The lines a client reads to show the log, at most `size`: its last, unless `cited` (0-based) is before them,
    /// when about half the window is before it and half after. The window starts earlier, at the outermost group
    /// holding `cited`, when that leaves a quarter of it after the line, so the group's folds read whole.
    pub fn window(&self, cited: Option<u64>, size: u64) -> LogWindow {
        let size = size.max(1);
        let tail = LineRange {
            from: self.lines.saturating_sub(size),
            to: self.lines,
        };
        let Some(line) = cited.filter(|line| *line < tail.from) else {
            return LogWindow::Tail(tail);
        };
        let reach = line.saturating_sub(size - size / 4);
        let mut from = line.saturating_sub(size / 2);
        let outermost = self
            .groups
            .iter()
            .filter(|group| group.start <= line && group.end.is_none_or(|end| end >= line))
            .map(|group| group.start)
            .min();
        if let Some(start) = outermost.filter(|start| *start < from && *start >= reach) {
            from = start;
        }
        LogWindow::Around {
            line,
            range: LineRange {
                from,
                to: (from + size).min(self.lines),
            },
        }
    }

    /// Why cited line `line` (0-based) is not among the lines read of this log: `line 9000 is past the log's end:
    /// it has 6020 lines`, `line 15 was deleted after history.logs`.
    pub fn absent_words(&self, line: u64) -> String {
        let number = line + 1;
        if line < self.lines {
            return match self.evidence {
                LogEvidence::Expired => format!("line {number} was deleted after history.logs"),
                _ => format!("line {number} is not in the stored log"),
            };
        }
        let has = Self::count_words(usize::try_from(self.lines).unwrap_or(usize::MAX));
        match self.evidence {
            LogEvidence::Missing => format!("line {number} is not there: no log was captured"),
            LogEvidence::Truncated => {
                format!(
                    "line {number} was not kept: the log was cut at history.log_max after {has}"
                )
            }
            LogEvidence::Running => {
                format!("line {number} is not printed yet: the log has {has} so far")
            }
            _ => format!("line {number} is past the log's end: it has {has}"),
        }
    }

    /// What a client says of cited line `line` (0-based) when the daemon is older than the outline and its lines,
    /// so it cannot read around it: `line 15 is not opened here; the full log has it`.
    pub fn unread_words(line: u64) -> String {
        format!("line {} is not opened here; the full log has it", line + 1)
    }
}
