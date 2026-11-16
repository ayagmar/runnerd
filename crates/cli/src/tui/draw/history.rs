//! The jobs view: a table of jobs, searched, filtered and sorted, a page at a time, or the timeline of what each slot
//! ran over the same range.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use runnerd_core::api::{self, Column, JobSummary, RecordMark};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::job::fit_details;
use super::overview::{badge, badge_width};
use super::timeline::timeline;
use super::{beside_when, bold, column_title, fit, heading, length, row_of, selected, table_hits};
use crate::human::{ago, bytes, cores_used, duration};
use crate::tui::app::{App, History, Lens};
use crate::tui::mouse::{Hits, List};

pub(super) fn history(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let filters = &app.history;
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let name = api::Section::History.title();
    heading(
        frame,
        title,
        name,
        history_note(
            filters,
            usize::from(title.width).saturating_sub(name.width() + 2),
        ),
        theme,
    );
    if filters.lens == Lens::Timeline {
        timeline(frame, body, app, now, theme);
        return;
    }
    let Some(list) = &filters.list else {
        frame.render_widget(Line::styled("reading…", theme.dim()), body);
        return;
    };
    let jobs = &list.page.jobs;
    if jobs.is_empty() {
        frame.render_widget(Line::styled("no jobs", theme.dim()), body);
        return;
    }
    let (body, pane) = beside_when(frame, body, theme, |width| {
        Columns::alike(jobs, now, body.width, width)
    });
    let columns = Columns::of(jobs, body.width, now);
    let rows = jobs.iter().enumerate().map(|(at, job)| {
        let mut cells = Vec::new();
        for (column, width) in &columns.shown {
            cells.push(match column {
                Column::Outcome => Line::from(badge(job, theme)),
                Column::Repository => Line::from(fit(&job.repository, *width)),
                Column::Job => Line::from(fit(&job.title().to_string(), *width)),
                Column::Branch => Line::from(Span::styled(
                    fit(job.branch.as_deref().unwrap_or_default(), *width),
                    theme.dim(),
                )),
                Column::Waited => {
                    Line::from(job.waited(now).map_or_else(String::new, duration)).right_aligned()
                }
                Column::Ran => {
                    Line::from(job.ran(now).map_or_else(String::new, duration)).right_aligned()
                }
                Column::MemoryPeak => {
                    Line::from(job.metrics.memory_peak.map_or_else(String::new, bytes))
                        .right_aligned()
                }
                Column::Cpu => {
                    Line::from(job.metrics.cpu_average.map_or_else(String::new, cores_used))
                        .right_aligned()
                }
                Column::Written => Line::from(written(job, theme)),
                Column::Record => Line::from(
                    job.record_mark
                        .and_then(|mark| Some(Span::styled(mark.words()?, theme.of(mark.tone()))))
                        .unwrap_or_default(),
                ),
                _ => Line::from(Span::styled(ago(job.timeline.queued_at, now), theme.dim()))
                    .right_aligned(),
            });
        }
        row_of(cells, at == filters.selected)
    });
    let header: Vec<_> = columns
        .shown
        .iter()
        .map(|(column, _)| {
            let title = column_title(*column);
            match column {
                Column::Outcome
                | Column::Repository
                | Column::Job
                | Column::Branch
                | Column::Record
                | Column::Written => Cell::from(title),
                _ => Cell::from(Line::from(title).right_aligned()),
            }
        })
        .collect();
    let widths: Vec<_> = columns
        .shown
        .iter()
        .map(|(column, width)| match column {
            Column::Job => Constraint::Min(length(*width)),
            _ => Constraint::Length(length(*width)),
        })
        .collect();
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(theme.dim()))
        .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(filters.selected));
    frame.render_stateful_widget(table, body, &mut state);
    table_hits(hits, body, List::History, state.offset(), jobs.len());
    if let (Some(pane), Some(job)) = (pane, jobs.get(filters.selected)) {
        job_pane(frame, pane, job, now, theme);
    }
}

/// The selected job's facts beside the list: its outcome, name and repository, then every column's figure by its
/// name, the ones the list leaves out included, its slot, and the key that opens it.
fn job_pane(frame: &mut Frame<'_>, area: Rect, job: &JobSummary, now: u64, theme: Theme) {
    let width = usize::from(area.width);
    let mut lines = vec![
        Line::from(badge(job, theme)),
        Line::styled(
            fit(&job.title().to_string(), width).trim_end().to_owned(),
            bold(),
        ),
        Line::raw(fit(&job.repository, width).trim_end().to_owned()),
        Line::raw(""),
    ];
    let mut facts: Vec<(Column, Span<'static>)> = Vec::new();
    if let Some(workflow) = &job.workflow {
        facts.push((Column::Workflow, Span::raw(workflow.clone())));
    }
    if let Some(branch) = &job.branch {
        facts.push((Column::Branch, Span::raw(branch.clone())));
    }
    facts.push((Column::Queued, Span::raw(ago(job.timeline.queued_at, now))));
    if let Some(waited) = job.waited(now) {
        facts.push((Column::Waited, Span::raw(duration(waited))));
    }
    if let Some(ran) = job.ran(now) {
        facts.push((Column::Ran, Span::raw(duration(ran))));
    }
    if let Some(peak) = job.metrics.memory_peak {
        facts.push((Column::MemoryPeak, Span::raw(bytes(peak))));
    }
    if let Some(cpu) = job.metrics.cpu_average {
        facts.push((Column::Cpu, Span::raw(cores_used(cpu))));
    }
    if let Some(disk) = job.metrics.disk {
        facts.push((
            Column::Written,
            Span::styled(disk.short(), theme.of(disk.tone())),
        ));
    }
    if let Some((mark, words)) = job.record_mark.and_then(|mark| Some((mark, mark.words()?))) {
        facts.push((Column::Record, Span::styled(words, theme.of(mark.tone()))));
    }
    let label_width = facts
        .iter()
        .map(|(column, _)| column.title().width())
        .max()
        .unwrap_or(0);
    for (column, value) in facts {
        let room = width.saturating_sub(label_width + 2);
        lines.push(Line::from(vec![
            Span::styled(format!("{:<label_width$}  ", column.title()), theme.dim()),
            Span::styled(fit(&value.content, room).trim_end().to_owned(), value.style),
        ]));
    }
    if let Some(runner) = &job.runner {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<label_width$}  ", "Slot"), theme.dim()),
            Span::raw(format!("{} {}", runner.profile, runner.slot)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("enter opens its page", theme.dim()));
    frame.render_widget(Paragraph::new(lines), area);
}

/// What a job wrote, in its tone: a floor reads in the background.
fn written(job: &JobSummary, theme: Theme) -> Span<'static> {
    job.metrics.disk.map_or_else(Span::default, |disk| {
        Span::styled(disk.short(), theme.of(disk.tone()))
    })
}

/// The table's columns at a width, each as wide as its widest cell and heading.
struct Columns {
    /// In the order drawn, with their widths.
    shown: Vec<(Column, usize)>,
}

impl Columns {
    /// Every column whose cells say something, as wide as its content; short of room the least needed go first
    /// (CPU, what a job wrote, the branch, the record mark, the memory peak), until the repository and the job show
    /// whole. The wait and when it was queued go only to keep the names [`NAMES_MIN`] cells; then the names are cut,
    /// the job keeping the larger part.
    fn of(jobs: &[JobSummary], width: u16, now: u64) -> Columns {
        let mut columns = Self::wanted(jobs, now);
        Self::fit(&mut columns, usize::from(width));
        Columns {
            shown: columns
                .into_iter()
                .map(|(column, width, _)| (column, width))
                .collect(),
        }
    }

    /// Whether the list shows the same columns at `width` as at `full`: the pane beside it opens only where it does.
    fn alike(jobs: &[JobSummary], now: u64, full: u16, width: u16) -> bool {
        let kinds = |width| -> Vec<Column> {
            Self::of(jobs, width, now)
                .shown
                .into_iter()
                .map(|(column, _)| column)
                .collect()
        };
        kinds(full) == kinds(width)
    }

    /// Each column whose cells say something, as wide as its content, with the order it gives way in: the highest
    /// first, 0 never.
    fn wanted(jobs: &[JobSummary], now: u64) -> Vec<(Column, usize, u8)> {
        let widest = |column: Column, cells: &dyn Fn(&JobSummary) -> usize| {
            jobs.iter()
                .map(cells)
                .max()
                .unwrap_or(0)
                .max(column_title(column).width())
        };
        let any = |cells: &dyn Fn(&JobSummary) -> bool| jobs.iter().any(cells);
        let mut columns: Vec<(Column, usize, u8)> = vec![
            (
                Column::Outcome,
                badge_width(jobs).max(column_title(Column::Outcome).len()),
                0,
            ),
            (
                Column::Repository,
                widest(Column::Repository, &|job| job.repository.width()),
                0,
            ),
            (
                Column::Job,
                widest(Column::Job, &|job| job.title().to_string().width()),
                0,
            ),
        ];
        if any(&|job| job.branch.is_some()) {
            columns.push((
                Column::Branch,
                widest(Column::Branch, &|job| {
                    job.branch.as_deref().unwrap_or_default().width()
                })
                .min(BRANCH_MOST),
                6,
            ));
        }
        columns.push((
            Column::Waited,
            widest(Column::Waited, &|job| {
                job.waited(now).map_or(0, |at| duration(at).width())
            }),
            2,
        ));
        columns.push((
            Column::Ran,
            widest(Column::Ran, &|job| {
                job.ran(now).map_or(0, |at| duration(at).width())
            }),
            0,
        ));
        if any(&|job| job.metrics.memory_peak.is_some()) {
            columns.push((
                Column::MemoryPeak,
                widest(Column::MemoryPeak, &|job| {
                    job.metrics
                        .memory_peak
                        .map_or(0, |peak| bytes(peak).width())
                }),
                3,
            ));
        }
        if any(&|job| job.metrics.cpu_average.is_some()) {
            columns.push((
                Column::Cpu,
                widest(Column::Cpu, &|job| {
                    job.metrics
                        .cpu_average
                        .map_or(0, |cpu| cores_used(cpu).width())
                }),
                8,
            ));
        }
        if any(&|job| job.metrics.disk.is_some()) {
            columns.push((
                Column::Written,
                widest(Column::Written, &|job| {
                    job.metrics.disk.map_or(0, |disk| disk.short().width())
                }),
                7,
            ));
        }
        if any(&|job| job.record_mark.and_then(RecordMark::words).is_some()) {
            columns.push((
                Column::Record,
                widest(Column::Record, &|job| {
                    job.record_mark
                        .and_then(RecordMark::words)
                        .map_or(0, str::width)
                }),
                4,
            ));
        }
        columns.push((
            Column::Queued,
            widest(Column::Queued, &|job| {
                ago(job.timeline.queued_at, now).width()
            }),
            1,
        ));
        columns
    }

    /// Drops and cuts `columns` to fit `width`.
    fn fit(columns: &mut Vec<(Column, usize, u8)>, width: usize) {
        let names = |columns: &[(Column, usize, u8)]| {
            columns
                .iter()
                .filter(|(column, _, _)| matches!(column, Column::Repository | Column::Job))
                .map(|(_, width, _)| *width)
                .sum::<usize>()
                + 1
        };
        // A name past a fair width is cut rather than cost the figures their columns.
        let wanted = names(columns).min(NAMES_FAIR);
        let room = |columns: &[(Column, usize, u8)]| {
            let others: usize = columns
                .iter()
                .filter(|(column, _, _)| !matches!(column, Column::Repository | Column::Job))
                .map(|(_, width, _)| width + 1)
                .sum();
            width.saturating_sub(others + 1)
        };
        loop {
            let left = room(columns);
            let least = columns
                .iter()
                .enumerate()
                .filter(|(_, (_, _, rank))| *rank > 2 || (*rank > 0 && left < NAMES_MIN))
                .max_by_key(|(_, (_, _, rank))| *rank)
                .map(|(at, _)| at);
            match least {
                Some(at) if left < wanted => {
                    columns.remove(at);
                }
                _ => break,
            }
        }
        let left = room(columns);
        if left < names(columns) {
            let repository = columns
                .iter()
                .find(|(column, _, _)| *column == Column::Repository)
                .map_or(0, |(_, width, _)| *width);
            let share = repository.min(left * 2 / 5);
            for (column, width, _) in columns.iter_mut() {
                match column {
                    Column::Repository => *width = share,
                    Column::Job => *width = left.saturating_sub(share + 1),
                    _ => {}
                }
            }
        }
    }
}

/// The cells the repository and the job may take together before a figure gives way to show them whole.
const NAMES_FAIR: usize = 72;

/// The cells the repository and the job share at the least, before the wait and when it was queued give way.
const NAMES_MIN: usize = 30;

/// The most cells a branch takes in the list; the pane beside it shows it whole.
const BRANCH_MOST: usize = 24;

/// What narrows the list and how it is read, as fit `width`: the search, each filter set, the order and range when
/// not the default, how much of a sorted list shows or the page; and where the daemon predates search and sort, that
/// they are not supported. Filters not set are left out.
pub(super) fn history_note(filters: &History, width: usize) -> String {
    let mut details = Vec::new();
    if filters.list.as_ref().is_some_and(|list| !list.understood()) {
        details.push((0, api::Feature::JobSearch.unsupported()));
    }
    if let Some(search) = &filters.search {
        details.push((0, format!("/{search}")));
    }
    for (column, set) in [
        (Column::Outcome, &filters.conclusion),
        (Column::Repository, &filters.repository),
        (Column::Workflow, &filters.workflow),
        (Column::Branch, &filters.branch),
    ] {
        if let Some(value) = set {
            details.push((1, format!("{} {value}", column.title().to_lowercase())));
        }
    }
    if let Some(record) = filters.record {
        details.push((1, record.words().to_owned()));
    }
    if filters.range != api::JobRange::All {
        details.push((1, filters.range.words().to_owned()));
    }
    let shown = filters.list.as_ref().and_then(api::JobList::shown_words);
    match shown {
        Some(shown) => details.push((2, shown)),
        None if !filters.order.pages() => details.push((2, filters.order.words().to_owned())),
        None if !filters.previous.is_empty() => {
            details.push((3, format!("page {}", filters.previous.len() + 1)));
        }
        None => {}
    }
    fit_details(details, width)
}
