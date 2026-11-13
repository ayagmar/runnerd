//! The jobs view: a table of jobs, searched, filtered and sorted, a page at a time, or the timeline of what each slot
//! ran over the same range.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table, TableState};
use runnerd_core::api::{self, Column, JobSummary};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::job::fit_details;
use super::overview::{badge, badge_width};
use super::timeline::timeline;
use super::{column_title, fit, heading, selected, table_hits};
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
    let wide = area.width >= 110;
    let columns = columns(jobs, area.width, now);
    // The repository and the job share what the other columns and the spaces between them leave.
    let fixed: usize = columns.iter().map(|(_, width)| width + 1).sum::<usize>() - 1;
    let shared = usize::from(body.width).saturating_sub(fixed);
    let repository_width = shared / 2;
    let job_width = shared - repository_width;
    let shown = |wanted: Column| columns.iter().any(|(column, _)| *column == wanted);
    let record_shown = shown(Column::Record);
    let rows = jobs.iter().map(|job| {
        let mut cells = vec![
            Cell::from(badge(job, theme)),
            Cell::from(fit(&job.repository, repository_width)),
            Cell::from(fit(&job.title().to_string(), job_width)),
        ];
        if wide {
            cells.push(Cell::from(Span::styled(
                fit(job.branch.as_deref().unwrap_or_default(), 14),
                theme.dim(),
            )));
        }
        if shown(Column::Waited) {
            cells.push(Cell::from(
                job.waited(now).map_or_else(String::new, duration),
            ));
        }
        cells.push(Cell::from(job.ran(now).map_or_else(String::new, duration)));
        if shown(Column::MemoryPeak) {
            cells.push(Cell::from(
                job.metrics.memory_peak.map_or_else(String::new, bytes),
            ));
        }
        if wide {
            cells.push(Cell::from(
                job.metrics.cpu_average.map_or_else(String::new, cores_used),
            ));
        }
        if record_shown {
            cells.push(Cell::from(
                job.record_mark
                    .and_then(|mark| Some(Span::styled(mark.words()?, theme.of(mark.tone()))))
                    .unwrap_or_default(),
            ));
        }
        if shown(Column::Queued) {
            cells.push(Cell::from(Span::styled(
                ago(job.timeline.queued_at, now),
                theme.dim(),
            )));
        }
        Row::new(cells)
    });
    let header: Vec<_> = columns
        .iter()
        .map(|(column, _)| column_title(*column))
        .collect();
    let widths: Vec<_> = columns
        .iter()
        .map(|(column, width)| match column {
            Column::Repository => Constraint::Length(u16::try_from(repository_width).unwrap_or(0)),
            Column::Job => Constraint::Fill(1),
            _ => Constraint::Length(u16::try_from(*width).unwrap_or(u16::MAX)),
        })
        .collect();
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(theme.dim()))
        .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(filters.selected));
    frame.render_stateful_widget(table, body, &mut state);
    table_hits(hits, body, List::History, state.offset(), jobs.len());
}

/// The table's columns and their widths at `width`: branch and CPU only when wide, the record mark when a job has one,
/// the memory peak, the wait and when it was queued only when the names keep [`NAMES_MIN`] cells.
fn columns(jobs: &[JobSummary], width: u16, now: u64) -> Vec<(Column, usize)> {
    let wide = width >= 110;
    let finished = jobs
        .iter()
        .map(|job| ago(job.timeline.queued_at, now).width())
        .chain([column_title(Column::Queued).len()])
        .max()
        .unwrap_or(0);
    let outcome_width = badge_width(jobs).max(column_title(Column::Outcome).len());
    let marks = jobs
        .iter()
        .filter_map(|job| job.record_mark?.words())
        .map(UnicodeWidthStr::width)
        .max();
    let mut columns = vec![
        (Column::Outcome, outcome_width),
        (Column::Repository, 0),
        (Column::Job, 0),
    ];
    if wide {
        columns.push((Column::Branch, 14));
    }
    columns.extend([
        (Column::Waited, 7),
        (Column::Ran, 8),
        (Column::MemoryPeak, column_title(Column::MemoryPeak).len()),
    ]);
    if wide {
        columns.push((Column::Cpu, 5));
    }
    if let Some(marks) = marks.filter(|_| width >= 90) {
        columns.push((
            Column::Record,
            marks.max(column_title(Column::Record).len()),
        ));
    }
    columns.push((Column::Queued, finished));
    // The repository and the job need room to be told apart: the least needed figures give way first.
    for least in [Column::MemoryPeak, Column::Waited, Column::Queued] {
        let fixed: usize = columns.iter().map(|(_, width)| width + 1).sum::<usize>() - 1;
        if usize::from(width).saturating_sub(fixed) >= NAMES_MIN {
            break;
        }
        columns.retain(|(column, _)| *column != least);
    }
    columns
}

/// The cells the repository and the job share at the least, before figures give way.
const NAMES_MIN: usize = 30;

/// What narrows the list and how it is read, as fit `width`: the search, each filter set, the order and range when
/// not the default, how much of a sorted list shows or the page; and where the daemon predates search and sort, that
/// they are not supported. Filters not set are left out.
pub(super) fn history_note(filters: &History, width: usize) -> String {
    let mut details = Vec::new();
    if filters.list.as_ref().is_some_and(|list| !list.understood()) {
        details.push((0, format!("search and sort {}", api::NOT_SUPPORTED)));
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
