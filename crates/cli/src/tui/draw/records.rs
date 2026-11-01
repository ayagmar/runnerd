//! The records view: the records of failed jobs, their sizes and the limits they are kept within.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use runnerd_core::api::{self, Column, RecordNote, Records};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::job::fit_details;
use super::{column_title, fit, heading, selected, styled_heading};
use crate::human::{self, bytes};
use crate::tui::app::App;

/// The records of failed jobs, each a stopped runner `runnerctl shell` opens a shell in.
pub(super) fn records(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let name = api::View::Records.title();
    let [title, table_area, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let Some(records) = &app.records else {
        heading(frame, title, name, String::new(), theme);
        frame.render_widget(Line::styled("reading…", theme.dim()), table_area);
        return;
    };
    // Near max_total, that comes first and stands out: the usage is what a narrow heading drops.
    match records.pressure() {
        Some(pressure) => {
            let room = usize::from(title.width).saturating_sub(name.width() + 2);
            let note = fit_details(vec![(0, pressure.to_owned()), (1, records.usage())], room);
            styled_heading(frame, title, name, note, theme.wait());
        }
        None => heading(frame, title, name, records.usage(), theme),
    }
    if records.records.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(Records::empty_reason(), theme.dim()))
                .wrap(Wrap { trim: true }),
            table_area,
        );
        return;
    }
    let made: Vec<String> = records
        .records
        .iter()
        .map(|record| {
            let mut made = human::ago(record.created_at, now);
            if records.shell_open(record.job) {
                made.push_str(", shell open");
            }
            made
        })
        .collect();
    let made_width = made.iter().map(|made| made.width()).max().unwrap_or(0);
    let fixed = [12, 6, made_width.max("MADE".len())];
    // The repository and the job's name share what the other columns and the spaces between them leave.
    let shared = usize::from(table_area.width).saturating_sub(fixed.iter().sum::<usize>() + 4);
    let repository_width = shared / 2;
    let name_width = shared - repository_width;
    let rows = records.records.iter().zip(made).map(|(record, made)| {
        Row::new(vec![
            Cell::from(record.job.to_string()),
            Cell::from(fit(&record.repository, repository_width)),
            Cell::from(fit(&record.title().to_string(), name_width)),
            Cell::from(bytes(record.bytes)),
            Cell::from(Span::styled(made, theme.dim())),
        ])
    });
    let length = |width: usize| Constraint::Length(u16::try_from(width).unwrap_or(u16::MAX));
    let table = Table::new(
        rows,
        [
            length(fixed[0]),
            length(repository_width),
            Constraint::Fill(1),
            length(fixed[1]),
            length(fixed[2]),
        ],
    )
    .header(
        Row::new(
            [
                Column::Id,
                Column::Repository,
                Column::Job,
                Column::Size,
                Column::Made,
            ]
            .map(column_title),
        )
        .style(theme.dim()),
    )
    .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(app.record));
    frame.render_stateful_widget(table, table_area, &mut state);
    if let Some(record) = records.records.get(app.record) {
        frame.render_widget(
            Line::styled(
                format!(
                    "{} opens a shell in its workspace",
                    RecordNote::shell_command(record.job)
                ),
                theme.dim(),
            ),
            hint,
        );
    }
}
