//! The open job's log as the TUI shows it: core's outline folded by step and group, the lines read so far, the row the
//! keys are on and the last search. It decides what is folded, which rows there are and which lines to read next;
//! reading is `tui`'s and drawing `draw::log`'s. Lines are core's styled lines: from `/log/lines`, or from the plain
//! log parsed by core when the daemon predates the outline.

use std::collections::{BTreeMap, BTreeSet};

use runnerd_core::api::{
    LINES_MAX, LineKind, LineRange, LogOutline, LogSearch, StepLog, StyledLine,
};

use super::reading::{Reading, Unread};

/// Lines kept of one log; past it, those farthest from the lines read last go.
pub(crate) const LOG_KEPT: usize = 20_000;

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
    cursor: Option<usize>,
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
        self.cursor
    }

    /// How many lines are read.
    pub(crate) fn read(&self) -> usize {
        self.lines.len()
    }

    /// Takes the outline: the failed step, or else the step that runs, opens, and a failed job's rows start at it.
    pub(crate) fn take_outline(&mut self, answer: Result<LogOutline, Unread>) {
        let first = self.outline.get().is_none();
        self.outline.take(answer);
        let Some(outline) = self.outline.get() else {
            return;
        };
        if !first {
            return;
        }
        let open = outline
            .failure
            .as_ref()
            .and_then(|failure| failure.step)
            .or_else(|| {
                outline
                    .evidence
                    .grows()
                    .then(|| {
                        outline
                            .steps
                            .iter()
                            .rev()
                            .find(|step| lines(step.lines).is_some())
                    })
                    .flatten()
                    .map(|step| step.number)
            });
        let sections = self.sections();
        let chosen = open.and_then(|number| {
            sections
                .iter()
                .position(|section| section.kind == SectionKind::Step(number))
        });
        match (chosen, sections.as_slice()) {
            (Some(at), _) => {
                if let Some(section) = sections.get(at) {
                    self.open.insert(Fold::Section(section.range.from));
                }
                if outline.failure.is_some() {
                    self.cursor = Some(
                        self.rows()
                            .iter()
                            .position(|row| *row == Row::Section(at))
                            .unwrap_or(0),
                    );
                }
            }
            // One part with nothing to choose between: it opens.
            (
                None,
                [
                    only,
                    Section {
                        kind: SectionKind::Flat,
                        ..
                    },
                ],
            ) => {
                self.open.insert(Fold::Section(only.range.from));
            }
            (None, _) => {}
        }
    }

    /// Takes lines read, in any order; past [`LOG_KEPT`], the lines farthest from them go.
    pub(crate) fn take_lines(&mut self, read: Vec<StyledLine>) {
        let Some(near) = read.first().map(|line| line.line) else {
            return;
        };
        for line in read {
            self.lines.insert(line.line, line);
        }
        let mut forgot = false;
        while self.lines.len() > LOG_KEPT {
            let (Some(first), Some(last)) = (
                self.lines.keys().next().copied(),
                self.lines.keys().next_back().copied(),
            ) else {
                break;
            };
            let gone = if near.abs_diff(first) > near.abs_diff(last) {
                first
            } else {
                last
            };
            self.lines.remove(&gone);
            forgot = true;
        }
        if forgot {
            // What went may be asked for again.
            self.asked.clear();
        }
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
                let next_read = self.lines.range(line..to).next().map(|(at, _)| *at);
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

    /// The ranges the rows lack that were not asked for yet, at most [`LINES_MAX`] lines each; they count as asked.
    pub(crate) fn wants(&mut self) -> Vec<LineRange> {
        let mut wanted = Vec::new();
        for row in self.rows() {
            let Row::Missing(range) = row else {
                continue;
            };
            let range = LineRange {
                from: range.from,
                to: range.to.min(range.from + u64::from(LINES_MAX)),
            };
            if self
                .asked
                .iter()
                .any(|asked| asked.from <= range.from && range.from < asked.to)
            {
                continue;
            }
            self.asked.push(range);
            wanted.push(range);
        }
        wanted
    }

    /// Moves the keys `by` rows; past the last, it follows the log again.
    pub(crate) fn step(&mut self, by: isize) {
        let len = self.rows().len();
        let at = self.cursor.unwrap_or(len.saturating_sub(1));
        let next = at.saturating_add_signed(by);
        self.cursor = (next < len.saturating_sub(1)).then_some(next);
    }

    /// To the first row.
    pub(crate) fn start(&mut self) {
        self.cursor = Some(0);
    }

    /// To the last row, following the log.
    pub(crate) fn end(&mut self) {
        self.cursor = None;
    }

    /// Opens or closes the section or group the keys are on.
    pub(crate) fn toggle(&mut self) {
        let rows = self.rows();
        let Some(row) = self
            .cursor
            .or_else(|| rows.len().checked_sub(1))
            .and_then(|at| rows.get(at).copied())
        else {
            return;
        };
        let fold = match row {
            Row::Section(at) => self
                .sections()
                .get(at)
                .map(|section| Fold::Section(section.range.from)),
            Row::Group(at) => self.group_start(at).map(Fold::Group),
            Row::Line(_) | Row::Missing(_) => None,
        };
        if let Some(fold) = fold {
            if !self.open.remove(&fold) {
                self.open.insert(fold);
            }
            self.cursor = rows.iter().position(|candidate| *candidate == row);
        }
    }

    /// Puts the keys on row `at`.
    pub(crate) fn point(&mut self, at: usize) {
        self.cursor = Some(at);
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
        let rows = self.rows();
        self.cursor = rows
            .iter()
            .position(|row| match row {
                Row::Line(at) => *at == line,
                Row::Missing(range) => range.from <= line && line < range.to,
                Row::Section(_) | Row::Group(_) => false,
            })
            .or(self.cursor);
    }
}

/// Which way to the next match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Way {
    Forward,
    Backward,
}

/// A step's lines, when the outline knows them and there are some.
fn lines(log: StepLog) -> Option<LineRange> {
    match log {
        StepLog::Lines(range) if !range.is_empty() => Some(range),
        StepLog::Lines(_) | StepLog::Empty | StepLog::Unattributed | StepLog::Unknown => None,
    }
}
