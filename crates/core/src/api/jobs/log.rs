//! A job's log as structure, for every client (ADR 0133): each stored line styled, the outline the store keeps as
//! the lines arrive, what a failed job's log says of its failure, and a literal search. The plain log
//! (`GET /v1/jobs/{id}/logs`) and `log.line` events carry the lines as stored; these types are the only way the log
//! reaches a client as structure, and no escape sequence or control character is left in them.
//!
//! A styled line has its timestamp split off, the runner's `##[…]` command classified, and its SGR colours mapped
//! to a closed set; every other escape and control character is dropped. Pure functions: the store feeds
//! [`Outliner`] and keeps what it returns, and the daemon, the demo's fake daemon and the fixtures assemble the same
//! answers from it.

use serde::{Deserialize, Serialize};

mod outline;
mod search;

pub(crate) use outline::TITLE_SOURCE_MAX;
pub(crate) use outline::title;
pub use outline::{
    FailureSummary, LineRange, LogEvidence, LogGroup, LogMark, LogOutline, MarkKind, OutlineChange,
    OutlineParts, Outliner, SavedOutline, StepLines, StepLog, StoredLog,
};
pub use search::{LogNeedle, LogSearch, LogSearchQuery, SEARCH_QUERY_MAX, Search};

/// `GET /v1/jobs/{id}/log/lines`: lines a page holds unless `limit` says otherwise.
pub const LINES_DEFAULT: u32 = 1000;

/// `GET /v1/jobs/{id}/log/lines`: lines a page holds at most.
pub const LINES_MAX: u32 = 10_000;

/// The most of a stored line's text a styled line shows, in bytes; the rest is counted in
/// [`StyledLine::omitted`].
pub const LINE_BYTES_MAX: u64 = 64 << 10;

/// The most runs a styled line holds; the text after them is counted in [`StyledLine::omitted`].
pub const RUNS_MAX: usize = 1000;

/// How the runner prints a line's command: `##[error]…`.
const COMMAND: &str = "##[";

/// Stored lines to read: up to `limit` from line `from` (past a gap pruning left, too), each cut to its first
/// `prefix` bytes, stopping after the line that brings the text read to `bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRead {
    /// The first line.
    pub from: u64,
    /// How many lines at most.
    pub limit: u64,
    /// How much of each line's text, in bytes.
    pub prefix: u64,
    /// How much text in all, in bytes: the line that reaches it is the last.
    pub bytes: u64,
}

impl LineRead {
    /// What it reads of `lines`, a whole log held in memory, as the store reads it: for fixtures and the demo.
    pub fn of(self, lines: &[String]) -> Vec<StoredLine> {
        let mut read = Vec::new();
        let mut bytes = 0;
        let first = usize::try_from(self.from).unwrap_or(usize::MAX);
        let numbered = (self.from..).zip(lines.iter().skip(first));
        for (line, text) in numbered.take(usize::try_from(self.limit).unwrap_or(usize::MAX)) {
            if bytes >= self.bytes {
                break;
            }
            let prefix = usize::try_from(self.prefix).unwrap_or(usize::MAX);
            let cut = &text.as_bytes()[..text.len().min(prefix)];
            let stored = StoredLine::cut(line, cut, text.len() as u64);
            bytes += stored.text.len() as u64;
            read.push(stored);
        }
        read
    }
}

/// A stored line as a [`LineRead`] gives it: its number, and its text, cut to the bytes the read asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLine {
    /// Its number in the stored log, from 0.
    pub line: u64,
    /// Its text, or the first of it; bytes that are not UTF-8 replaced.
    pub text: String,
    /// The stored bytes not in `text`.
    pub omitted: u64,
}

impl StoredLine {
    /// Line `line` from the first `read` of its `stored` bytes: a character the cut splits is left out.
    pub fn cut(line: u64, read: &[u8], stored: u64) -> StoredLine {
        let whole = if (read.len() as u64) < stored {
            whole_chars(read)
        } else {
            read
        };
        StoredLine {
            line,
            text: String::from_utf8_lossy(whole).into_owned(),
            omitted: stored.saturating_sub(whole.len() as u64),
        }
    }

    /// Styled, what it omits counted.
    pub fn styled(&self) -> StyledLine {
        let mut styled = StyledLine::parse(self.line, &self.text);
        styled.omitted += self.omitted;
        styled
    }
}

/// `bytes` without a UTF-8 character cut off at its end.
fn whole_chars(bytes: &[u8]) -> &[u8] {
    // The last character starts at most 3 bytes before the end, at a byte that is not a continuation byte.
    let Some(start) = (bytes.len().saturating_sub(4)..bytes.len())
        .rev()
        .find(|&at| bytes[at] & 0xC0 != 0x80)
    else {
        return bytes;
    };
    let width = match bytes[start] {
        lead if lead >= 0xF0 => 4,
        lead if lead >= 0xE0 => 3,
        lead if lead >= 0xC0 => 2,
        _ => 1,
    };
    if start + width > bytes.len() {
        &bytes[..start]
    } else {
        bytes
    }
}

/// The first `max` bytes of `text`, or fewer so a character is not cut.
pub(crate) fn prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `GET /v1/jobs/{id}/log/lines` query parameters. The response is `application/x-ndjson`: one [`StyledLine`] per
/// line, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLinesQuery {
    /// Start at this line, 0-based.
    pub from: Option<u64>,
    /// At most this many lines; default [`LINES_DEFAULT`], at most [`LINES_MAX`]. Not read with `follow`.
    pub limit: Option<u32>,
    /// Keep the response open and send lines as they are stored, until the job finishes, as the plain log does.
    #[serde(default)]
    pub follow: bool,
}

impl LogLinesQuery {
    /// How many lines a page without `follow` holds.
    pub fn page(&self) -> u64 {
        u64::from(self.limit.unwrap_or(LINES_DEFAULT).clamp(1, LINES_MAX))
    }
}

/// What a line is, from the command the runner prefixed it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineKind {
    /// What a step printed.
    Output,
    /// `##[group]`: a foldable group starts; the runs are its title.
    Group,
    /// `##[endgroup]`: the innermost open group ends.
    EndGroup,
    /// `##[error]`.
    Error,
    /// `##[warning]`.
    Warning,
    /// `##[notice]`.
    Notice,
    /// `##[debug]`.
    Debug,
    /// `##[command]`: a command an action ran, such as `/usr/bin/git version`.
    Command,
    /// A kind a newer daemon names; this client draws it as output.
    #[serde(other)]
    Unknown,
}

/// A colour a run of text was printed in, from the closed set a log is drawn with. Bright and dark shades are one
/// colour; white, default and 256-colour or RGB colours beyond the first 16 have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Color {
    /// Red.
    Red,
    /// Green.
    Green,
    /// Yellow.
    Yellow,
    /// Blue.
    Blue,
    /// Magenta.
    Magenta,
    /// Cyan.
    Cyan,
    /// Black or bright black: dimmed text.
    Grey,
    /// A colour a newer daemon names; this client draws it uncoloured.
    #[serde(other)]
    Unknown,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !value
}

/// A run of a line's text in one style.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// The text, without escapes or control characters but tabs.
    pub text: String,
    /// Its colour; none for the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    /// Printed bold.
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    /// A search's match: the text the query found.
    #[serde(default, skip_serializing_if = "is_false")]
    pub mark: bool,
}

/// One stored line of a job's log, styled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyledLine {
    /// Its number in the stored log, from 0: the same as the plain log's.
    pub line: u64,
    /// When the runner wrote it, in seconds since the Unix epoch; absent for a line without a timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    /// What it is.
    pub kind: LineKind,
    /// Its text after the timestamp and the command, in order: at most [`RUNS_MAX`] runs of the line's first
    /// [`LINE_BYTES_MAX`] bytes.
    pub runs: Vec<Run>,
    /// Bytes of the stored line not shown: past [`LINE_BYTES_MAX`] or [`RUNS_MAX`], or past what a read took.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl StyledLine {
    /// Styles stored line number `line`, whose text is `text`: its first [`LINE_BYTES_MAX`] bytes, in at most
    /// [`RUNS_MAX`] runs.
    pub fn parse(line: u64, text: &str) -> StyledLine {
        let shown = prefix(text, usize::try_from(LINE_BYTES_MAX).unwrap_or(usize::MAX));
        let (at, rest) = split_time(shown);
        let (kind, rest) = command(rest);
        let (runs, read) = runs(rest, RUNS_MAX);
        StyledLine {
            line,
            at: at.map(|ms| ms / 1000),
            kind,
            runs,
            omitted: (text.len() - shown.len() + rest.len() - read) as u64,
        }
    }

    /// Its text, without styles.
    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }
}

/// A line's timestamp in ms, when it starts with one as the runner writes it (`2026-10-23T01:08:56.1234567Z `),
/// and the rest.
fn split_time(text: &str) -> (Option<u64>, &str) {
    match text.split_once(' ') {
        Some((stamp, rest)) => match crate::time::parse_ms(stamp) {
            Some(at) => (Some(at), rest),
            None => (None, text),
        },
        None => match crate::time::parse_ms(text) {
            Some(at) => (Some(at), ""),
            None => (None, text),
        },
    }
}

/// A line's command, after its timestamp, and the rest; output for a command the runner does not name.
fn command(text: &str) -> (LineKind, &str) {
    let Some((name, rest)) = text
        .strip_prefix(COMMAND)
        .and_then(|after| after.split_once(']'))
    else {
        return (LineKind::Output, text);
    };
    let kind = match name {
        "group" => LineKind::Group,
        "endgroup" => LineKind::EndGroup,
        "error" => LineKind::Error,
        "warning" => LineKind::Warning,
        "notice" => LineKind::Notice,
        "debug" => LineKind::Debug,
        "command" => LineKind::Command,
        _ => return (LineKind::Output, text),
    };
    (kind, rest)
}

/// What a line is and its text after the timestamp and command, unstyled: what the outline reads.
pub(crate) fn classify(text: &str) -> (LineKind, &str) {
    command(split_time(text).1)
}

/// A line's time in seconds, when it has one.
pub(crate) fn time_of(text: &str) -> Option<u64> {
    split_time(text).0.map(|ms| ms / 1000)
}

/// Text without escapes or control characters but tabs, as a group's title or a search compares it.
pub(crate) fn plain(text: &str) -> String {
    runs(text, usize::MAX)
        .0
        .into_iter()
        .map(|run| run.text)
        .collect()
}

/// The style a run is printed in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    color: Option<Color>,
    bold: bool,
}

/// One of the eight basic colours, 0 to 7, as a log is drawn: white has none.
fn basic(index: u16) -> Option<Color> {
    match index {
        0 => Some(Color::Grey),
        1 => Some(Color::Red),
        2 => Some(Color::Green),
        3 => Some(Color::Yellow),
        4 => Some(Color::Blue),
        5 => Some(Color::Magenta),
        6 => Some(Color::Cyan),
        _ => None,
    }
}

impl Style {
    /// The style after an SGR sequence with `params`, such as `1;31`.
    fn apply(self, params: &str) -> Style {
        let codes: Vec<u16> = params
            .split([';', ':'])
            .map(|code| code.parse().unwrap_or(0))
            .collect();
        let mut style = self;
        let mut codes = codes.into_iter();
        while let Some(code) = codes.next() {
            match code {
                0 => style = Style::default(),
                1 => style.bold = true,
                22 => style.bold = false,
                30..=37 => style.color = basic(code - 30),
                90..=97 => style.color = basic(code - 90),
                39 => style.color = None,
                38 | 48 => {
                    let color = match codes.next() {
                        Some(5) => codes.next().and_then(|index| {
                            basic(if index >= 8 { index - 8 } else { index }).filter(|_| index < 16)
                        }),
                        Some(2) => {
                            codes.by_ref().take(3).for_each(drop);
                            None
                        }
                        _ => None,
                    };
                    if code == 38 {
                        style.color = color;
                    }
                }
                _ => {}
            }
        }
        style
    }
}

/// A line's text as at most `max` runs, and how many of its bytes they cover: SGR sequences become styles, every
/// other escape sequence and control character but a tab is dropped, and adjacent runs of one style are one.
fn runs(text: &str, max: usize) -> (Vec<Run>, usize) {
    let mut runs: Vec<Run> = Vec::new();
    let mut style = Style::default();
    let mut current = String::new();
    // Where `current`'s text starts in `text`.
    let mut began = 0;
    let mut chars = text.char_indices().peekable();
    // Adds `current` in `style`; false when that takes a run past `max`.
    let flush = |runs: &mut Vec<Run>, current: &mut String, style: Style| {
        if current.is_empty() {
            return true;
        }
        let full = runs.len() >= max;
        match runs.last_mut() {
            Some(last) if last.color == style.color && last.bold == style.bold => {
                last.text.push_str(current);
                current.clear();
            }
            _ if full => return false,
            _ => runs.push(Run {
                text: std::mem::take(current),
                color: style.color,
                bold: style.bold,
                mark: false,
            }),
        }
        true
    };
    while let Some((at, c)) = chars.next() {
        match c {
            '\u{1b}' => match chars.next().map(|(_, c)| c) {
                // CSI: parameters, then a final byte from `@` to `~`; only `m` (SGR) is kept, as a style.
                Some('[') => {
                    let mut params = String::new();
                    let mut last = None;
                    for (_, c) in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            last = Some(c);
                            break;
                        }
                        params.push(c);
                    }
                    if last == Some('m') {
                        let next = style.apply(&params);
                        if next != style {
                            if !flush(&mut runs, &mut current, style) {
                                return (runs, began);
                            }
                            style = next;
                        }
                    }
                }
                // OSC, such as a hyperlink or a title: up to BEL or ST.
                Some(']') => {
                    while let Some((_, c)) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next_if(|&(_, c)| c == '\\');
                            break;
                        }
                    }
                }
                // Any other escape is two characters.
                _ => {}
            },
            c if c == '\t' || !c.is_control() => {
                if current.is_empty() {
                    began = at;
                }
                current.push(c);
            }
            _ => {}
        }
    }
    if flush(&mut runs, &mut current, style) {
        (runs, text.len())
    } else {
        (runs, began)
    }
}

// The tests read the wire format, JSON, which only the host and client features read.
#[cfg(test)]
#[cfg(any(feature = "host", feature = "client"))]
mod tests;
