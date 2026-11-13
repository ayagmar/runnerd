//! The caches view: one row per cache volume, what it holds, its size against its limit, then the build cache's week.
//! Emptying one is `d`, asked first.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use runnerd_core::api::{self, Caches};
use unicode_width::UnicodeWidthStr;

use super::{Theme, fit, heading, length, selected, table_hits, widths};
use crate::human::{ago, bytes};
use crate::tui::app::App;
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

pub(super) fn caches(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let name = api::Section::Caches.title();
    let [title, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let caches: &Caches = match &app.caches {
        Reading::Read(caches) => caches,
        Reading::Unread(why) => {
            heading(frame, title, name, String::new(), theme);
            let text = match why {
                Unread::Waiting => "reading…".to_owned(),
                Unread::Unsupported => api::NOT_SUPPORTED.to_owned(),
                Unread::Failed(why) => why.clone(),
            };
            frame.render_widget(Line::styled(text, theme.dim()), rest);
            return;
        }
    };
    let Some(measured) = caches.measured_at else {
        heading(frame, title, name, String::new(), theme);
        frame.render_widget(
            Line::styled(
                "not measured yet: runnerd measures caches at start and when runners are idle",
                theme.dim(),
            ),
            rest,
        );
        return;
    };
    heading(
        frame,
        title,
        name,
        format!("measured {}", ago(measured, now)),
        theme,
    );
    let build: Vec<String> = caches
        .build
        .as_ref()
        .map(|build| {
            vec![
                format!("Build cache for {}", api::BUILD_CACHE_TOOLS),
                build.week_line(),
                build.actions_cache_line(),
            ]
        })
        .unwrap_or_default();
    let [table_area, build_area] = Layout::vertical([
        Constraint::Length(length(caches.volumes.len() + 1)),
        Constraint::Min(0),
    ])
    .areas(rest);
    let holds_width = caches
        .volumes
        .iter()
        .map(|volume| volume.kind.description().width())
        .max()
        .unwrap_or(0)
        .min(40);
    let columns = [
        ("VOLUME", 0),
        ("HOLDS", holds_width),
        ("SIZE", 8),
        ("LIMIT", 18),
    ];
    let rows = caches.volumes.iter().map(|volume| {
        let limit = volume.limit_status();
        Row::new(vec![
            Cell::from(volume.name.clone()),
            Cell::from(Span::styled(
                fit(&volume.kind.description(), holds_width),
                theme.dim(),
            )),
            Cell::from(Span::styled(
                bytes(volume.bytes),
                limit
                    .as_ref()
                    .map_or_else(ratatui::style::Style::new, |limit| theme.of(limit.tone())),
            )),
            Cell::from(limit.map(|limit| limit.label).unwrap_or_default()),
        ])
    });
    let table = Table::new(rows, widths(&columns))
        .header(Row::new(columns.iter().map(|(name, _)| *name)).style(theme.dim()))
        .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(app.cache));
    frame.render_stateful_widget(table, table_area, &mut state);
    table_hits(
        hits,
        table_area,
        List::Caches,
        state.offset(),
        caches.volumes.len(),
    );
    let lines = build_lines(build, usize::from(build_area.width));
    frame.render_widget(Paragraph::new(lines), build_area);
}

/// The build cache's lines under the table: its heading in bold, then each line wrapped, never cut, since each says
/// what the cache does as a whole sentence.
fn build_lines(build: Vec<String>, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    for (at, line) in build.into_iter().enumerate() {
        let style = if at == 0 {
            super::bold()
        } else {
            ratatui::style::Style::new()
        };
        lines.extend(
            super::wrap(&line, width)
                .into_iter()
                .map(|line| Line::styled(line, style)),
        );
    }
    lines
}
