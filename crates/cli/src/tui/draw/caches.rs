//! The caches view: one row per cache volume, what it holds, its size against its limit, then the build cache's week
//! as core's figures.
//! Emptying one is `d`, asked first.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, TableState};
use runnerd_core::api::{self, Caches};

use super::budget::scale;
use super::{TableColumn, Theme, fitted_table, heading, length, table_hits};
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
                Unread::Unsupported => api::Feature::Caches.unsupported(),
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
    let [table_area, build_area] = Layout::vertical([
        Constraint::Length(length(caches.volumes.len() + 1)),
        Constraint::Min(0),
    ])
    .areas(rest);
    let columns = columns(&caches.volumes, theme);
    let table = fitted_table(
        columns,
        usize::from(table_area.width),
        Some(app.cache),
        theme,
    );
    let mut state = TableState::default().with_selected(Some(app.cache));
    frame.render_stateful_widget(table, table_area, &mut state);
    table_hits(
        hits,
        table_area,
        List::Caches,
        state.offset(),
        caches.volumes.len(),
    );
    let lines = caches
        .build
        .as_ref()
        .map(|build| build_lines(build, usize::from(build_area.width), theme))
        .unwrap_or_default();
    let lines = super::clip(lines, usize::from(build_area.height), theme);
    frame.render_widget(Paragraph::new(lines), build_area);
}

/// The cells of a volume's fill bar.
const FILL: u16 = 12;

/// How full a volume is against its limit, as a bar in its limit's tone; blank for a volume without one.
fn fill(volume: &api::CacheVolume, theme: Theme) -> Span<'static> {
    let Some(limit) = volume.limit_status() else {
        return Span::raw("");
    };
    let filled = scale(volume.bytes, limit.limit, FILL);
    Span::styled(
        format!(
            "{}{}",
            "█".repeat(usize::from(filled)),
            "░".repeat(usize::from(FILL - filled))
        ),
        theme.of(limit.tone()),
    )
}

/// The build cache's lines under the table: its heading in bold, its week as core's figures, then how GitHub
/// Actions' cache goes through it, wrapped, never cut, since it is a whole sentence.
fn build_lines(build: &api::BuildCacheStats, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    lines.extend(
        super::wrap(
            &format!("Build cache for {}", api::BUILD_CACHE_TOOLS),
            width,
        )
        .into_iter()
        .map(|line| Line::styled(line, super::bold())),
    );
    lines.extend(super::figure_rows(
        build.figures().iter().map(Into::into),
        width,
        theme,
    ));
    lines.extend(
        super::wrap(&build.actions_cache_line(), width)
            .into_iter()
            .map(Line::from),
    );
    lines
}

/// The table's columns: each volume's name, what it holds, its size in its limit's tone, how full it is, and its
/// limit.
fn columns(volumes: &[api::CacheVolume], theme: Theme) -> Vec<TableColumn> {
    vec![
        TableColumn::names(
            "Volume",
            volumes
                .iter()
                .map(|volume| Span::raw(volume.name.clone()))
                .collect(),
        ),
        TableColumn::words(
            "Holds",
            volumes
                .iter()
                .map(|volume| Span::styled(volume.kind.description(), theme.dim()))
                .collect(),
            2,
        ),
        TableColumn::figures(
            "Size",
            volumes
                .iter()
                .map(|volume| {
                    let style = volume
                        .limit_status()
                        .map_or_else(Style::new, |limit| theme.of(limit.tone()));
                    Span::styled(bytes(volume.bytes), style)
                })
                .collect(),
            0,
        ),
        TableColumn::words(
            "",
            volumes.iter().map(|volume| fill(volume, theme)).collect(),
            3,
        ),
        TableColumn::words(
            "Limit",
            volumes
                .iter()
                .map(|volume| {
                    Span::raw(
                        volume
                            .limit_status()
                            .map(|limit| limit.label)
                            .unwrap_or_default(),
                    )
                })
                .collect(),
            1,
        ),
    ]
}
