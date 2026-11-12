//! The open job's log as the TUI shows it: core's outline folded by step and group, the lines read so far, the row the
//! keys are on and the last search. It decides what is folded, which rows there are and which lines to read next;
//! reading is `tui`'s and drawing `draw::log`'s. Lines are core's styled lines: from `/log/lines`, or from the plain
//! log parsed by core when the daemon predates the outline.

use std::collections::{BTreeMap, BTreeSet};

use runnerd_core::api::{LineKind, LineRange, LogOutline, LogSearch, StepLog, StyledLine};

use super::reading::{Reading, Unread};

/// Lines kept of one log; past it, those farthest from the row the keys are on go.
pub(crate) const LOG_KEPT: usize = 20_000;

/// Lines asked for at once: a few screens either side of the row the keys are on.
const PAGE: u64 = 1_000;

/// How many rows either side of the keys' row are read when they are not yet: more than a screen shows.
const NEAR: usize = 100;

/// A part of the log the keys can fold, by its first line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fold {
    Section(u64),
    Group(u64),
}

/// What a section of the log is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectionKind {
    /// A step's lines, by its number.
    Step(u32),
    /// Lines no step got.
    Unattributed,
    /// Every line, without a header: no outline, or the lines past it.
    Flat,
}

/// Lines `range` of the log, one header that folds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Section {
    pub(crate) kind: SectionKind,
    /// `to` is `u64::MAX` for lines still to come.
    pub(crate) range: LineRange,
}

/// The row the keys are on, by what it shows rather than its place, so rows that come or go above it leave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// A section's header, by its first line.
    Section(u64),
    /// A group's header, by its first line.
    Group(u64),
    /// A line, read or not yet.
    Line(u64),
}

impl At {
    /// The line it stands at.
    fn line(self) -> u64 {
        match self {
            At::Section(line) | At::Group(line) | At::Line(line) => line,
        }
    }
}

/// One row of the log on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row {
    /// A section's header, by its place in [`LogView::sections`].
    Section(usize),
    /// A group's header, by its place in the outline's groups.
    Group(usize),
    Line(u64),
    /// Lines not read yet.
    Missing(LineRange),
}

/// A search of the log and the match the keys are on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    query: String,
    matches: Vec<u64>,
    /// Its matched lines, with their matches marked.
    lines: BTreeMap<u64, StyledLine>,
    /// The match shown, once one is.
    at: Option<usize>,
    /// Where the next page starts, when there is one.
    next: Option<u64>,
}

impl Found {
    fn new(query: String) -> Found {
        Found {
            query,
            matches: Vec::new(),
            lines: BTreeMap::new(),
            at: None,
            next: None,
        }
    }

    /// Takes a page; returns whether it brought the first match to show.
    fn take(&mut self, page: LogSearch) -> bool {
        self.matches.extend(page.matches);
        self.next = page.next;
        for line in page.lines {
            self.lines.insert(line.line, line);
        }
        self.at.is_none() && !self.matches.is_empty()
    }

    /// Moves to the next match or the one before, wrapping; returns its line.
    fn step(&mut self, way: Way) -> Option<u64> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        let at = match (self.at, way) {
            (None, _) => 0,
            (Some(at), Way::Forward) => (at + 1) % len,
            (Some(at), Way::Backward) => (at + len - 1) % len,
        };
        self.at = Some(at);
        self.matches.get(at).copied()
    }

    /// What was searched for.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// The matches read so far.
    pub(crate) fn count(&self) -> usize {
        self.matches.len()
    }

    /// The match shown, counted from 1; 0 before one is.
    pub(crate) fn shown(&self) -> usize {
        self.at.map_or(0, |at| at + 1)
    }

    /// Whether more pages of matches may follow.
    pub(crate) fn has_more(&self) -> bool {
        self.next.is_some()
    }

    /// Where the next page starts, when the match shown is the last one read and there is one.
    pub(crate) fn next_page(&self) -> Option<u64> {
        let next = self.next?;
        (self.at? + 1 == self.matches.len()).then_some(next)
    }
}

/// The open job's log. Replaced whole when another job opens; nothing survives leaving the job or the TUI.
#[derive(Debug, Default)]
pub(crate) struct LogView {
    outline: Reading<LogOutline>,
    lines: BTreeMap<u64, StyledLine>,
    open: BTreeSet<Fold>,
    /// Ranges asked for and not since forgotten, so a range is asked once.
    asked: Vec<LineRange>,
    /// The row the keys are on; `None` follows the last.
    cursor: Option<At>,
    found: Option<Found>,
}

impl LogView {
    pub(crate) fn outline(&self) -> &Reading<LogOutline> {
        &self.outline
    }

    pub(crate) fn found(&self) -> Option<&Found> {
        self.found.as_ref()
    }

    /// The row the keys are on; `None` while it follows the last.
    pub(crate) fn cursor(&self) -> Option<usize> {
        self.cursor_in(&self.rows())
    }

    /// The place in `rows`, as [`LogView::rows`] gives them, of the row the keys are on: the row it was, or, when
    /// that is folded away, the row nearest its line. `None` while it follows the last.
    pub(crate) fn cursor_in(&self, rows: &[Row]) -> Option<usize> {
        let at = self.cursor?;
        let sections = self.sections();
        rows.iter()
            .position(
                |row| match (self.at_of(&sections, *row, Way::Forward), at) {
                    (Some(At::Line(_)), At::Line(line)) => covers(*row, line),
                    (shown, _) => shown == Some(at),
                },
            )
            .or_else(|| {
                rows.iter()
                    .enumerate()
                    .min_by_key(|(_, row)| distance(**row, &sections, self, at.line()))
                    .map(|(place, _)| place)
            })
    }

    /// What `row` shows; a row of lines not read yet stands at its first line, or its last when reached from below.
    fn at_of(&self, sections: &[Section], row: Row, way: Way) -> Option<At> {
        match row {
            Row::Section(at) => sections
                .get(at)
                .map(|section| At::Section(section.range.from)),
            Row::Group(at) => self.group_start(at).map(At::Group),
            Row::Line(line) => Some(At::Line(line)),
            Row::Missing(range) => Some(At::Line(match way {
                Way::Forward => range.from,
                Way::Backward => range.to.saturating_sub(1),
            })),
        }
    }

    /// Puts the keys on `row` of `rows`.
    fn put(&mut self, rows: &[Row], at: usize, way: Way) {
        let sections = self.sections();
        self.cursor = rows
            .get(at)
            .and_then(|row| self.at_of(&sections, *row, way));
    }

    /// How many lines are read.
    pub(crate) fn read(&self) -> usize {
        self.lines.len()
    }

    /// Takes the outline. On the first, a failed job's rows start at its failed step, open: or, when that step has no
    /// lines of its own, at the last part that has. A log of one part opens it. While the log grows and the keys follow
    /// it, every outline opens the last step with lines and its groups that have not ended, so what comes shows.
    pub(crate) fn take_outline(&mut self, answer: Result<LogOutline, Unread>) {
        let first = self.outline.get().is_none();
        self.outline.take(answer);
        let Some(outline) = self.outline.get() else {
            return;
        };
        let follows = outline.evidence.grows() && self.cursor.is_none();
        let failed = outline.failure.as_ref().map(|failure| failure.step);
        let running = follows
            .then(|| {
                outline
                    .steps
                    .iter()
                    .rev()
                    .find_map(|step| lines(step.lines))
            })
            .flatten();
        let unended: Vec<u64> = if follows {
            outline
                .groups
                .iter()
                .filter(|group| group.end.is_none())
                .map(|group| group.start)
                .collect()
        } else {
            Vec::new()
        };
        if let Some(running) = running {
            self.open.insert(Fold::Section(running.from));
        }
        for start in unended {
            self.open.insert(Fold::Group(start));
        }
        if !first {
            return;
        }
        let sections = self.sections();
        if let Some(step) = failed {
            let section = sections
                .iter()
                .find(|section| step.is_some_and(|step| section.kind == SectionKind::Step(step)))
                .or_else(|| {
                    sections
                        .iter()
                        .rev()
                        .find(|section| section.kind != SectionKind::Flat)
                });
            if let Some(section) = section {
                self.open.insert(Fold::Section(section.range.from));
                self.cursor = Some(At::Section(section.range.from));
            }
        } else if running.is_none()
            && let [
                only,
                Section {
                    kind: SectionKind::Flat,
                    ..
                },
            ] = sections.as_slice()
        {
            // One part with nothing to choose between: it opens.
            self.open.insert(Fold::Section(only.range.from));
        }
    }

    /// Takes lines read, in any order. Past [`LOG_KEPT`], the kept lines farthest in rows from the keys' row go,
    /// those no row shows (folded away) first, as [`LogView::wants`] measures nearness in rows too: a line it would
    /// read again is never the one forgotten. No line of `read` goes, and only the ranges asked that lost a line may
    /// be asked again.
    pub(crate) fn take_lines(&mut self, read: Vec<StyledLine>) {
        let (Some(first), Some(last)) = (
            read.iter().map(|line| line.line).min(),
            read.iter().map(|line| line.line).max(),
        ) else {
            return;
        };
        for line in read {
            self.lines.insert(line.line, line);
        }
        let excess = self.lines.len().saturating_sub(LOG_KEPT);
        if excess == 0 {
            return;
        }
        let rows = self.rows();
        let keys = self.keys_row(&rows);
        let mut shown: BTreeMap<u64, usize> = BTreeMap::new();
        for (place, row) in rows.iter().enumerate() {
            if let Row::Line(line) = row {
                shown.insert(*line, place.abs_diff(keys));
            }
        }
        let mut candidates: Vec<(usize, u64)> = self
            .lines
            .keys()
            .filter(|line| !(first..=last).contains(*line))
            .map(|line| (shown.get(line).copied().unwrap_or(usize::MAX), *line))
            .collect();
        // Farthest first; of lines as far, those farthest from the keys' line.
        let near = self.cursor.map_or(u64::MAX, At::line);
        candidates.sort_by_key(|(rows, line)| std::cmp::Reverse((*rows, near.abs_diff(*line))));
        let gone: BTreeSet<u64> = candidates
            .into_iter()
            .take(excess)
            .map(|(_, line)| line)
            .collect();
        for line in &gone {
            self.lines.remove(line);
        }
        // What went may be asked for again; nothing else.
        self.asked
            .retain(|asked| gone.range(asked.from..asked.to).next().is_none());
    }

    /// The place in `rows` of the keys' row: the last while they follow.
    fn keys_row(&self, rows: &[Row]) -> usize {
        self.cursor_in(rows).unwrap_or(rows.len().saturating_sub(1))
    }

    /// The parts of the log in order: each step's lines and those no step got, each under a header that folds them,
    /// then the lines past the outline, as they come. Without an outline, every line in one part with no header.
    pub(crate) fn sections(&self) -> Vec<Section> {
        let flat = |from| Section {
            kind: SectionKind::Flat,
            range: LineRange { from, to: u64::MAX },
        };
        let Some(outline) = self.outline.get() else {
            return vec![flat(0)];
        };
        let mut sections: Vec<Section> = outline
            .steps
            .iter()
            .filter_map(|step| {
                Some(Section {
                    kind: SectionKind::Step(step.number),
                    range: lines(step.lines)?,
                })
            })
            .chain(
                outline
                    .unattributed
                    .iter()
                    .filter(|range| !range.is_empty())
                    .map(|range| Section {
                        kind: SectionKind::Unattributed,
                        range: *range,
                    }),
            )
            .collect();
        sections.sort_by_key(|section| section.range.from);
        if sections.is_empty() {
            return vec![flat(0)];
        }
        sections.push(flat(outline.lines));
        sections
    }

    /// The rows on the screen: each section's header, and in an open one its lines, a group's header for each group
    /// with its lines when it is open, and a row for each run of lines not read yet. `##[endgroup]` lines are left
    /// out: the group's header says where it ends.
    pub(crate) fn rows(&self) -> Vec<Row> {
        let groups: BTreeMap<u64, usize> = self
            .outline
            .get()
            .map(|outline| {
                outline
                    .groups
                    .iter()
                    .enumerate()
                    .map(|(at, group)| (group.start, at))
                    .collect()
            })
            .unwrap_or_default();
        let ends: Vec<Option<u64>> = self
            .outline
            .get()
            .map(|outline| outline.groups.iter().map(|group| group.end).collect())
            .unwrap_or_default();
        let mut rows = Vec::new();
        for (at, section) in self.sections().into_iter().enumerate() {
            let to = if section.kind == SectionKind::Flat {
                // As far as lines are read.
                match self.last_line() {
                    Some(last) if last >= section.range.from => last + 1,
                    _ => continue,
                }
            } else {
                rows.push(Row::Section(at));
                if !self.open.contains(&Fold::Section(section.range.from)) {
                    continue;
                }
                section.range.to
            };
            let mut line = section.range.from;
            while line < to {
                if let Some(group) = groups.get(&line).copied() {
                    rows.push(Row::Group(group));
                    line = if self.open.contains(&Fold::Group(line)) {
                        line + 1
                    } else {
                        ends[group].map_or(to, |end| end + 1)
                    };
                    continue;
                }
                if let Some(read) = self.line(line) {
                    if read.kind != LineKind::EndGroup {
                        rows.push(Row::Line(line));
                    }
                    line += 1;
                    continue;
                }
                // A search's match is a row even where the lines around it are not read yet.
                let next_read = [&self.lines, self.found_lines()]
                    .into_iter()
                    .filter_map(|lines| lines.range(line..to).next().map(|(at, _)| *at))
                    .min();
                let next_group = groups.range(line..to).next().map(|(at, _)| *at);
                let until = [next_read, next_group, Some(to)]
                    .into_iter()
                    .flatten()
                    .min()
                    .unwrap_or(to);
                rows.push(Row::Missing(LineRange {
                    from: line,
                    to: until,
                }));
                line = until;
            }
        }
        rows
    }

    fn last_line(&self) -> Option<u64> {
        self.lines.keys().next_back().copied()
    }

    fn found_lines(&self) -> &BTreeMap<u64, StyledLine> {
        static NONE: BTreeMap<u64, StyledLine> = BTreeMap::new();
        self.found.as_ref().map_or(&NONE, |found| &found.lines)
    }

    /// Line `line` as read, with a search's matches marked while the search stands.
    pub(crate) fn line(&self, line: u64) -> Option<&StyledLine> {
        self.found
            .as_ref()
            .and_then(|found| found.lines.get(&line))
            .or_else(|| self.lines.get(&line))
    }

    /// Whether a section or group header is open.
    pub(crate) fn is_open(&self, row: Row) -> bool {
        match row {
            Row::Section(at) => self
                .sections()
                .get(at)
                .is_some_and(|section| self.open.contains(&Fold::Section(section.range.from))),
            Row::Group(at) => self
                .group_start(at)
                .is_some_and(|start| self.open.contains(&Fold::Group(start))),
            Row::Line(_) | Row::Missing(_) => true,
        }
    }

    fn group_start(&self, at: usize) -> Option<u64> {
        self.outline.get()?.groups.get(at).map(|group| group.start)
    }

    /// The lines to read next, not asked for yet, at most [`PAGE`] each; they count as asked. Only rows of lines not
    /// read yet within [`NEAR`] rows of the keys are read: the page around the keys' line when it is not read, the
    /// page next to the lines shown otherwise.
    pub(crate) fn wants(&mut self) -> Vec<LineRange> {
        let rows = self.rows();
        if rows.is_empty() {
            return Vec::new();
        }
        let at = self.keys_row(&rows);
        let line = self.cursor.map(At::line);
        let mut wanted = Vec::new();
        for (place, row) in rows
            .iter()
            .enumerate()
            .take(at.saturating_add(NEAR).saturating_add(1))
            .skip(at.saturating_sub(NEAR))
        {
            let Row::Missing(range) = *row else {
                continue;
            };
            let target = match line {
                Some(line) if range.from <= line && line < range.to => line,
                _ if place < at => range.to.saturating_sub(1),
                _ => range.from,
            };
            let page = page_around(range, target);
            if self
                .asked
                .iter()
                .any(|asked| asked.from < page.to && page.from < asked.to)
            {
                continue;
            }
            self.asked.push(page);
            wanted.push(page);
        }
        wanted
    }

    /// A read of `range` failed: it may be asked for again.
    pub(crate) fn unask(&mut self, range: LineRange) {
        self.asked.retain(|asked| *asked != range);
    }

    /// Moves the keys `by` rows; past the last, it follows the log again.
    pub(crate) fn step(&mut self, by: isize) {
        let rows = self.rows();
        let len = rows.len();
        let at = self.cursor_in(&rows).unwrap_or(len.saturating_sub(1));
        let next = at.saturating_add_signed(by);
        if next < len.saturating_sub(1) {
            let way = if by < 0 { Way::Backward } else { Way::Forward };
            self.put(&rows, next, way);
        } else {
            self.cursor = None;
        }
    }

    /// To the first row.
    pub(crate) fn start(&mut self) {
        let rows = self.rows();
        self.put(&rows, 0, Way::Forward);
        if self.cursor.is_none() {
            self.cursor = Some(At::Line(0));
        }
    }

    /// To the last row, following the log.
    pub(crate) fn end(&mut self) {
        self.cursor = None;
    }

    /// Opens or closes the section or group the keys are on.
    pub(crate) fn toggle(&mut self) {
        let rows = self.rows();
        let Some(at) = self.cursor_in(&rows).or_else(|| rows.len().checked_sub(1)) else {
            return;
        };
        let sections = self.sections();
        let fold = match rows.get(at) {
            Some(Row::Section(at)) => sections
                .get(*at)
                .map(|section| Fold::Section(section.range.from)),
            Some(Row::Group(at)) => self.group_start(*at).map(Fold::Group),
            Some(Row::Line(_) | Row::Missing(_)) | None => None,
        };
        if let Some(fold) = fold {
            if !self.open.remove(&fold) {
                self.open.insert(fold);
            }
            self.put(&rows, at, Way::Forward);
        }
    }

    /// Puts the keys on row `at`.
    pub(crate) fn point(&mut self, at: usize) {
        let rows = self.rows();
        self.put(&rows, at, Way::Forward);
    }

    /// A new search: the matches come with [`LogView::take_found`].
    pub(crate) fn search(&mut self, query: String) {
        self.found = Some(Found::new(query));
    }

    /// Ends the search; its marks go.
    pub(crate) fn clear_search(&mut self) {
        self.found = None;
    }

    /// Takes a page of the search of `query`, then shows its first match if none is shown yet. Returns the line
    /// shown, for its lines around it to be read.
    pub(crate) fn take_found(&mut self, query: &str, page: LogSearch) -> Option<u64> {
        let found = self.found.as_mut().filter(|found| found.query == query)?;
        if !found.take(page) {
            return None;
        }
        self.next_match(Way::Forward)
    }

    /// Shows the next match after the one shown, or the one before, wrapping; opens what folds it. Returns its line.
    pub(crate) fn next_match(&mut self, way: Way) -> Option<u64> {
        let line = self.found.as_mut()?.step(way)?;
        self.reveal(line);
        Some(line)
    }

    /// Opens the section and the groups that hold `line`, and puts the keys on it.
    fn reveal(&mut self, line: u64) {
        if let Some(section) = self.sections().into_iter().find(|section| {
            section.kind != SectionKind::Flat
                && section.range.from <= line
                && line < section.range.to
        }) {
            self.open.insert(Fold::Section(section.range.from));
        }
        if let Some(outline) = self.outline.get() {
            for group in &outline.groups {
                if group.start < line && group.end.is_none_or(|end| line <= end) {
                    self.open.insert(Fold::Group(group.start));
                }
            }
        }
        self.cursor = Some(At::Line(line));
    }
}

/// Which way to the next match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Way {
    Forward,
    Backward,
}

/// Whether `row` shows `line`, read or not yet.
fn covers(row: Row, line: u64) -> bool {
    match row {
        Row::Line(at) => at == line,
        Row::Missing(range) => range.from <= line && line < range.to,
        Row::Section(_) | Row::Group(_) => false,
    }
}

/// How many lines `row` stands from `line`.
fn distance(row: Row, sections: &[Section], view: &LogView, line: u64) -> u64 {
    match row {
        Row::Section(at) => sections
            .get(at)
            .map_or(u64::MAX, |section| section.range.from.abs_diff(line)),
        Row::Group(at) => view
            .group_start(at)
            .map_or(u64::MAX, |start| start.abs_diff(line)),
        Row::Line(at) => at.abs_diff(line),
        Row::Missing(_) if covers(row, line) => 0,
        Row::Missing(range) => range.from.abs_diff(line).min(range.to.abs_diff(line)),
    }
}

/// At most [`PAGE`] lines of `range` around `line`.
fn page_around(range: LineRange, line: u64) -> LineRange {
    let from = range.from.max(line.saturating_sub(PAGE / 2));
    let to = range.to.min(from.saturating_add(PAGE));
    LineRange {
        from: range.from.max(to.saturating_sub(PAGE)),
        to,
    }
}

/// A step's lines, when the outline knows them and there are some.
fn lines(log: StepLog) -> Option<LineRange> {
    match log {
        StepLog::Lines(range) if !range.is_empty() => Some(range),
        StepLog::Lines(_) | StepLog::Empty | StepLog::Unattributed | StepLog::Unknown => None,
    }
}
