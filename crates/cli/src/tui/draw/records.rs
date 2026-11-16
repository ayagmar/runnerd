//! The records view: the records of failed jobs, their sizes and the limits they are kept within.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, TableState, Wrap};
use runnerd_core::api::{self, Column, RecordNote, Records};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::job::fit_details;
use super::{TableColumn, fitted_table, heading, styled_heading, table_hits};
use crate::human::{self, bytes};
use crate::tui::app::App;
use crate::tui::mouse::{Hits, List};

/// The records of failed jobs, each a stopped runner `runnerctl shell` opens a shell in.
pub(super) fn records(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let name = api::Section::Records.title();
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
    let plain = |text: &dyn Fn(&api::Record) -> String| -> Vec<Span<'static>> {
        records
            .records
            .iter()
            .map(|record| Span::raw(text(record)))
            .collect()
    };
    let columns = vec![
        TableColumn::words(
            Column::Id.title(),
            plain(&|record| record.job.to_string()),
            0,
        ),
        TableColumn::names(
            Column::Repository.title(),
            plain(&|record| record.repository.clone()),
        ),
        TableColumn::names(
            Column::Job.title(),
            plain(&|record| record.title().to_string()),
        ),
        TableColumn::figures(
            Column::Size.title(),
            plain(&|record| bytes(record.bytes)),
            0,
        ),
        TableColumn::words(
            Column::Made.title(),
            made.into_iter()
                .map(|made| Span::styled(made, theme.dim()))
                .collect(),
            0,
        ),
    ];
    let table = fitted_table(
        columns,
        usize::from(table_area.width),
        Some(app.record),
        theme,
    );
    let mut state = TableState::default().with_selected(Some(app.record));
    frame.render_stateful_widget(table, table_area, &mut state);
    table_hits(
        hits,
        table_area,
        List::Records,
        state.offset(),
        records.records.len(),
    );
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
