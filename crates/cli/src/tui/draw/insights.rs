//! The insights view: what the job minutes would have cost on GitHub's runners, then what the history says, one row
//! per insight in its tone, the selected one's whole sentence under them, and the host's memory on the days CI ran.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api;
use unicode_width::UnicodeWidthStr;

use super::{Theme, bold, fit, heading, length, selected, wrap};
use crate::tui::app::App;
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

pub(super) fn insights(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: Theme,
    hits: &mut Hits,
) {
    let name = api::View::Insights.title();
    let insights = match &app.insights {
        Reading::Read(insights) => insights,
        Reading::Unread(why) => {
            let [title, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
            heading(frame, title, name, String::new(), theme);
            let text = match why {
                Unread::Waiting => "reading…".to_owned(),
                Unread::Unsupported => api::Feature::Insights.unsupported(),
                Unread::Failed(why) => why.clone(),
            };
            frame.render_widget(Line::styled(text, theme.dim()), rest);
            return;
        }
    };
    let width = usize::from(area.width);
    let savings: Vec<String> = app
        .savings
        .as_ref()
        .map(|savings| wrap(&savings.month.headline(), width))
        .unwrap_or_default();
    let selected_insight = insights.insights.get(app.insight);
    let detail: Vec<String> = selected_insight
        .map(|insight| wrap(&insight.sentence, width))
        .unwrap_or_default();
    let host: Vec<String> = insights
        .host
        .iter()
        .map(api::HostHazard::dated_line)
        .collect();
    let rows = area.height.saturating_sub(1);
    // Whole or not at all: its basis and what it leaves out stay with the dollars.
    let savings_height = if length(savings.len()) <= rows / 4 {
        length(savings.len())
    } else {
        0
    };
    let detail_height = length(detail.len() + 1).min(rows / 3);
    let host_height = if host.is_empty() {
        0
    } else {
        length(host.len() + 1).min(rows / 4)
    };
    let [title, savings_area, list_area, detail_area, host_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(savings_height),
        Constraint::Min(1),
        Constraint::Length(detail_height),
        Constraint::Length(host_height),
    ])
    .areas(area);
    heading(
        frame,
        title,
        name,
        insights.warning_line().map_or_else(
            || insights.window_line(),
            |line| format!("{line} {}", insights.window_line()),
        ),
        theme,
    );
    frame.render_widget(
        Paragraph::new(savings.into_iter().map(Line::from).collect::<Vec<_>>()),
        savings_area,
    );
    if insights.insights.is_empty() {
        frame.render_widget(Line::styled(insights.empty_line(), theme.dim()), list_area);
        return;
    }
    insight_rows(frame, list_area, app, insights, theme, hits);
    // The selected insight's whole sentence, which its row cuts.
    let mut detail_lines = vec![Line::from("")];
    detail_lines.extend(detail.into_iter().map(Line::from));
    frame.render_widget(Paragraph::new(detail_lines), detail_area);
    if host_height > 0 {
        let mut lines = vec![Line::styled("Host memory while CI ran", bold())];
        lines.extend(host.into_iter().map(|line| Line::styled(line, theme.dim())));
        frame.render_widget(Paragraph::new(lines), host_area);
    }
}

/// A row per insight: a dot and its kind in its tone, then its sentence, cut; the selected one stays in sight.
fn insight_rows(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    insights: &api::Insights,
    theme: Theme,
    hits: &mut Hits,
) {
    let width = usize::from(area.width);
    let label_width = insights
        .insights
        .iter()
        .map(|insight| insight.kind.label().width())
        .max()
        .unwrap_or(0);
    let shown = usize::from(area.height);
    let first = app.insight.saturating_sub(shown.saturating_sub(1));
    let lines: Vec<Line<'_>> = insights
        .insights
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, insight)| {
            let rest = width.saturating_sub(2 + label_width + 2);
            let line = Line::from(vec![
                Span::styled("● ", theme.of(insight.tone())),
                Span::styled(
                    format!("{:<label_width$}  ", insight.kind.label()),
                    theme.of(insight.tone()),
                ),
                Span::raw(fit(&insight.sentence, rest)),
            ]);
            if at == app.insight {
                line.style(selected())
            } else {
                line
            }
        })
        .collect();
    hits.rows(area, List::Insights, first, insights.insights.len() - first);
    frame.render_widget(Paragraph::new(lines), area);
}
