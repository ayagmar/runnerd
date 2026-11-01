//! The history view: finished jobs, filtered, a page at a time.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table, TableState};
use runnerd_core::api::{self, Column};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::job::fit_details;
use super::overview::{badge, badge_width};
use super::{column_title, fit, heading, selected};
use crate::human::{ago, bytes, cores_used, duration};
use crate::tui::app::{App, History};

pub(super) fn history(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let filters = &app.history;
    let [title, table_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    heading(
        frame,
        title,
        api::View::History.title(),
        history_note(
            filters,
            usize::from(title.width).saturating_sub(api::View::History.title().width() + 2),
        ),
        theme,
    );
    let Some(jobs) = &filters.page else {
        frame.render_widget(Line::styled("reading…", theme.dim()), table_area);
        return;
    };
    if jobs.jobs.is_empty() {
        frame.render_widget(Line::styled("no jobs", theme.dim()), table_area);
        return;
    }
    let wide = area.width >= 110;
    let finished = jobs
        .jobs
        .iter()
        .map(|job| ago(job.timeline.queued_at, now).width())
        .chain([column_title(Column::Queued).len()])
        .max()
        .unwrap_or(0);
    let outcome_width = badge_width(&jobs.jobs).max(column_title(Column::Outcome).len());
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
    columns.push((Column::Queued, finished));
    // The repository and the job share what the other columns and the spaces between them leave.
    let fixed: usize = columns.iter().map(|(_, width)| width + 1).sum::<usize>() - 1;
    let shared = usize::from(table_area.width).saturating_sub(fixed);
    let repository_width = shared / 2;
    let job_width = shared - repository_width;
    let rows = jobs.jobs.iter().map(|job| {
        let mut cells = vec![
            Cell::from(badge(job, theme)),
            Cell::from(fit(&job.repository, repository_width)),
            Cell::from(fit(&job.title().to_string(), job_width)),
        ];
        if wide {
            cells.push(Cell::from(fit(
                job.branch.as_deref().unwrap_or_default(),
                14,
            )));
        }
        cells.extend([
            Cell::from(job.waited(now).map_or_else(String::new, duration)),
            Cell::from(job.ran(now).map_or_else(String::new, duration)),
            Cell::from(job.metrics.memory_peak.map_or_else(String::new, bytes)),
        ]);
        if wide {
            cells.push(Cell::from(
                job.metrics.cpu_average.map_or_else(String::new, cores_used),
            ));
        }
        cells.push(Cell::from(Span::styled(
            ago(job.timeline.queued_at, now),
            theme.dim(),
        )));
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
    frame.render_stateful_widget(table, table_area, &mut state);
}

/// The history's filters and page, as fit `width`.
pub(super) fn history_note(filters: &History, width: usize) -> String {
    let any = |filter: &Option<String>| filter.clone().unwrap_or_else(|| "any".to_owned());
    fit_details(
        vec![
            (0, format!("outcome {}", any(&filters.conclusion))),
            (0, format!("repository {}", any(&filters.repository))),
            (1, format!("workflow {}", any(&filters.workflow))),
            (2, format!("page {}", filters.previous.len() + 1)),
        ],
        width,
    )
}
