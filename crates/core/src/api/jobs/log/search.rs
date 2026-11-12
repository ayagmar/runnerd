//! A literal search of a job's stored log: which lines hold a query, with the lines around them, a page at a time.
//! [`Search`] reads the log in batches its caller fetches, so no request reads more than [`SCAN_LINES`] lines or
//! [`SCAN_BYTES`] bytes of text, and says where the next page continues. A search does not follow a growing log: a
//! client searches again.
//!
//! It searches a line's message as [`StyledLine`] shows it, not the stored text: without the runner's timestamp, its
//! `##[…]` command and escape sequences, and only the first [`super::LINE_BYTES_MAX`] bytes of a line. `error` finds
//! a line that says "error", not every `##[error]` line; the outline's marks list those.

use std::collections::VecDeque;
use std::ops::Range;

use serde::{Deserialize, Serialize};

use super::{LINE_BYTES_MAX, LineRead, RUNS_MAX, Run, StoredLine, StyledLine};

/// The longest query, in bytes.
pub const SEARCH_QUERY_MAX: usize = 200;

/// Matches a page holds unless `limit` says otherwise.
pub(crate) const MATCHES_DEFAULT: u32 = 50;

/// Matches a page holds at most.
pub(crate) const MATCHES_MAX: u32 = 200;

/// Lines of context before and after each match, at most.
pub(crate) const CONTEXT_MAX: u32 = 5;

/// Lines one request scans at most.
pub(crate) const SCAN_LINES: u64 = 50_000;

/// Bytes of lines one request scans at most.
pub(crate) const SCAN_BYTES: u64 = 4 << 20;

/// Lines asked for at a time.
const BATCH: u64 = 500;

/// `GET /v1/jobs/{id}/log/search` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSearchQuery {
    /// What to find in each line's message: literal text, 1 to 200 bytes; ASCII letters match in either case. The
    /// message is the line as styled, without its timestamp, `##[…]` command or escapes: label it as such.
    pub q: String,
    /// Scan from this line: the `next` of the previous page.
    pub from: Option<u64>,
    /// At most this many matches; default 50, at most 200.
    pub limit: Option<u32>,
    /// Lines to show before and after each match; default 0, at most 5.
    pub context: Option<u32>,
    /// Refused: a search does not follow a log yet.
    #[serde(default)]
    pub follow: bool,
}

/// `GET /v1/jobs/{id}/log/search`: one page of the lines whose message holds a query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSearch {
    /// The lines that hold the query, in order.
    pub matches: Vec<u64>,
    /// The matching lines, their runs split so the query's text is marked, and the context around them, in order,
    /// each once.
    pub lines: Vec<StyledLine>,
    /// Where the next page continues, while stored lines remain unscanned; absent once every line stored when it
    /// was read was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

/// A query, ready to find.
#[derive(Debug, Clone)]
pub struct LogNeedle {
    /// The query, its ASCII letters lowercase.
    lower: String,
}

impl LogNeedle {
    /// The needle for `query`, or why it cannot be one.
    pub fn new(query: &str) -> Result<LogNeedle, String> {
        if query.is_empty() {
            return Err("q is empty: say what to find".to_owned());
        }
        if query.len() > SEARCH_QUERY_MAX {
            return Err(format!("q is longer than {SEARCH_QUERY_MAX} bytes"));
        }
        Ok(LogNeedle {
            lower: query.to_ascii_lowercase(),
        })
    }

    /// Whether `line` holds the query; if so, its runs are split so each stretch of matches is one marked run.
    /// Past [`RUNS_MAX`] runs the rest of the line is counted in `omitted`, not shown.
    pub fn mark(&self, line: &mut StyledLine) -> bool {
        let text = line.text();
        // Lowercasing ASCII keeps every byte where it is, so a match is at a character boundary of `text` too.
        let lower = text.to_ascii_lowercase();
        let mut found: Vec<Range<usize>> = Vec::new();
        for (at, matched) in lower.match_indices(self.lower.as_str()) {
            match found.last_mut() {
                Some(last) if last.end == at => last.end = at + matched.len(),
                _ => found.push(at..at + matched.len()),
            }
        }
        if found.is_empty() {
            return false;
        }
        let mut runs: Vec<Run> = Vec::new();
        let mut found = found.into_iter().peekable();
        let mut offset = 0;
        'runs: for run in &line.runs {
            let end = offset + run.text.len();
            let mut at = offset;
            while at < end {
                while found.next_if(|range| range.end <= at).is_some() {}
                let (to, mark) = match found.peek() {
                    Some(range) if range.start <= at => (range.end.min(end), true),
                    Some(range) => (range.start.min(end), false),
                    None => (end, false),
                };
                let full = runs.len() >= RUNS_MAX;
                match runs.last_mut() {
                    Some(last)
                        if (last.color, last.bold, last.mark) == (run.color, run.bold, mark) =>
                    {
                        last.text.push_str(&text[at..to]);
                    }
                    _ if full => {
                        line.omitted += (text.len() - at) as u64;
                        break 'runs;
                    }
                    _ => runs.push(Run {
                        text: text[at..to].to_owned(),
                        color: run.color,
                        bold: run.bold,
                        mark,
                    }),
                }
                at = to;
            }
            offset = end;
        }
        line.runs = runs;
        true
    }
}

/// Where a search is.
#[derive(Debug)]
enum Stage {
    /// Scanning from `next`.
    Scanning { next: u64 },
    /// Done scanning, `left` lines of the last match's context still to read from `from`; the page then continues
    /// at `resume`.
    Trailing {
        from: u64,
        left: u32,
        resume: Option<u64>,
    },
    /// Done; the next page continues at `resume`.
    Done { resume: Option<u64> },
}

/// One page of a search, fed the log in the batches it asks for:
/// `while let Some(read) = search.wants() { search.read(lines(read)) }`, then [`Search::finish`].
#[derive(Debug)]
pub struct Search {
    needle: LogNeedle,
    limit: usize,
    context: u32,
    found: LogSearch,
    /// Lines read since the last one kept, at most `context` of them, for the next match's context.
    before: VecDeque<StyledLine>,
    /// Lines of the last match's context still to keep.
    after: u32,
    scanned_lines: u64,
    scanned_bytes: u64,
    stage: Stage,
}

impl Search {
    /// A search for `query`, or why it cannot be made.
    pub fn new(query: &LogSearchQuery) -> Result<Search, String> {
        if query.follow {
            return Err("a search does not follow a log: search again as it grows".to_owned());
        }
        Ok(Search {
            needle: LogNeedle::new(&query.q)?,
            limit: usize::try_from(query.limit.unwrap_or(MATCHES_DEFAULT).clamp(1, MATCHES_MAX))
                .unwrap_or(usize::MAX),
            context: query.context.unwrap_or(0).min(CONTEXT_MAX),
            found: LogSearch::default(),
            before: VecDeque::new(),
            after: 0,
            scanned_lines: 0,
            scanned_bytes: 0,
            stage: Stage::Scanning {
                next: query.from.unwrap_or(0),
            },
        })
    }

    /// What to read next; `None` once the page is complete. Scanning reads what is left of the budget; the last
    /// match's context, at most 5 lines past it.
    pub fn wants(&self) -> Option<LineRead> {
        match self.stage {
            Stage::Scanning { next } => Some(LineRead {
                from: next,
                limit: BATCH.min(SCAN_LINES - self.scanned_lines),
                prefix: LINE_BYTES_MAX,
                bytes: SCAN_BYTES - self.scanned_bytes,
            }),
            Stage::Trailing { from, left, .. } => Some(LineRead {
                from,
                limit: u64::from(left),
                prefix: LINE_BYTES_MAX,
                bytes: u64::MAX,
            }),
            Stage::Done { .. } => None,
        }
    }

    /// Takes the lines read as [`Search::wants`] asked; fewer than asked, short of its bytes, means the log ends.
    pub fn read(&mut self, lines: Vec<StoredLine>) {
        let asked = self.wants();
        let text: u64 = lines.iter().map(|line| line.text.len() as u64).sum();
        let short =
            asked.is_some_and(|read| (lines.len() as u64) < read.limit && text < read.bytes);
        let mut lines = lines.into_iter();
        if let Stage::Scanning { .. } = self.stage {
            for stored in lines.by_ref() {
                let number = stored.line;
                self.scanned_lines += 1;
                self.scanned_bytes += stored.text.len() as u64;
                let mut line = stored.styled();
                let matched = self.needle.mark(&mut line);
                if matched {
                    self.found.matches.push(number);
                    self.found.lines.extend(self.before.drain(..));
                    self.found.lines.push(line);
                    self.after = self.context;
                } else if self.after > 0 {
                    self.found.lines.push(line);
                    self.after -= 1;
                } else if self.context > 0 {
                    self.before.push_back(line);
                    if self.before.len() > self.context as usize {
                        self.before.pop_front();
                    }
                }
                let full = matched && self.found.matches.len() >= self.limit;
                if full || self.scanned_lines >= SCAN_LINES || self.scanned_bytes >= SCAN_BYTES {
                    self.stop(Some(number + 1));
                    break;
                }
                self.stage = Stage::Scanning { next: number + 1 };
            }
            if let Stage::Scanning { .. } = self.stage
                && short
            {
                self.stop(None);
            }
        }
        // What a stop left of the batch, or the lines read for it, is the last match's context.
        if let Stage::Trailing { resume, .. } = self.stage {
            for stored in lines.by_ref().take(self.after as usize) {
                let number = stored.line;
                self.found.lines.push(stored.styled());
                self.after -= 1;
                self.stage = Stage::Trailing {
                    from: number + 1,
                    left: self.after,
                    resume,
                };
            }
            if self.after == 0 || short {
                self.stage = Stage::Done { resume };
            }
        }
    }

    fn stop(&mut self, resume: Option<u64>) {
        self.stage = match (self.after, resume) {
            (0, _) | (_, None) => Stage::Done { resume },
            (left, Some(from)) => Stage::Trailing { from, left, resume },
        };
    }

    /// The page.
    pub fn finish(self) -> LogSearch {
        let resume = match self.stage {
            Stage::Scanning { next } => Some(next),
            Stage::Trailing { resume, .. } | Stage::Done { resume } => resume,
        };
        LogSearch {
            next: resume,
            ..self.found
        }
    }
}
