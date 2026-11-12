//! A literal search of a job's stored log: which lines hold a query, with the lines around them, a page at a time.
//! [`Search`] reads the log in batches its caller fetches, so no request reads more than [`SCAN_LINES`] lines or
//! [`SCAN_BYTES`] bytes, and says where the next page continues. A search does not follow a growing log: a client
//! searches again.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::{Run, StyledLine};

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
    /// What to find: literal text, 1 to 200 bytes; ASCII letters match in either case.
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

/// `GET /v1/jobs/{id}/log/search`: one page of matches.
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
    bytes: Vec<u8>,
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
            bytes: query.as_bytes().to_vec(),
        })
    }

    /// Whether `line` holds the query; if so, its runs are split so each match is a marked run.
    pub fn mark(&self, line: &mut StyledLine) -> bool {
        let text = line.text();
        let hay = text.as_bytes();
        let size = self.bytes.len();
        let mut found = Vec::new();
        let mut at = 0;
        while at + size <= hay.len() {
            if hay[at..at + size].eq_ignore_ascii_case(&self.bytes) {
                found.push(at..at + size);
                at += size;
            } else {
                at += 1;
            }
        }
        if found.is_empty() {
            return false;
        }
        // A match starts at an ASCII or leading byte and ends after a whole character: both are char boundaries.
        let mut runs = Vec::new();
        let mut offset = 0;
        for run in std::mem::take(&mut line.runs) {
            let end = offset + run.text.len();
            let mut cut = offset;
            let piece = |from: usize, to: usize, mark: bool, runs: &mut Vec<Run>| {
                if from < to {
                    runs.push(Run {
                        text: text[from..to].to_owned(),
                        mark,
                        ..run.clone()
                    });
                }
            };
            for range in found
                .iter()
                .filter(|range| range.start < end && range.end > offset)
            {
                let (start, stop) = (range.start.max(offset), range.end.min(end));
                piece(cut, start, false, &mut runs);
                piece(start, stop, true, &mut runs);
                cut = stop;
            }
            piece(cut, end, false, &mut runs);
            offset = end;
        }
        line.runs = runs;
        true
    }
}

/// What [`Search`] wants read next: up to `limit` stored lines from line `from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchRead {
    /// The first line.
    pub from: u64,
    /// How many lines at most.
    pub limit: u64,
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

    /// What to read next; `None` once the page is complete.
    pub fn wants(&self) -> Option<SearchRead> {
        match self.stage {
            Stage::Scanning { next } => Some(SearchRead {
                from: next,
                limit: BATCH,
            }),
            Stage::Trailing { from, left, .. } => Some(SearchRead {
                from,
                limit: u64::from(left),
            }),
            Stage::Done { .. } => None,
        }
    }

    /// Takes the lines read as [`Search::wants`] asked, with their numbers; fewer than asked means the log ends.
    pub fn read(&mut self, lines: Vec<(u64, String)>) {
        let asked = self.wants().map_or(0, |read| read.limit);
        let short = (lines.len() as u64) < asked;
        let mut lines = lines.into_iter();
        if let Stage::Scanning { .. } = self.stage {
            for (number, text) in lines.by_ref() {
                self.scanned_lines += 1;
                self.scanned_bytes += text.len() as u64 + 1;
                let mut line = StyledLine::parse(number, &text);
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
            for (number, text) in lines.by_ref().take(self.after as usize) {
                self.found.lines.push(StyledLine::parse(number, &text));
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
