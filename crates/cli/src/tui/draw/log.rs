//! One job's log, following it or scrolled.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use runnerd_core::api::LogSummary;

use super::Theme;
use super::bold;
use crate::human::Tone;
use crate::tui::app::JobView;

pub(super) fn log(frame: &mut Frame<'_>, area: Rect, open: &JobView, theme: Theme) {
    let summary = &open.job.summary.log;
    let following = if open.scroll == 0 {
        "following".to_owned()
    } else {
        format!("{} lines up, G follows", open.scroll)
    };
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(theme.dim())
        .title(Span::styled(" Log ", bold()))
        .title_top(
            Line::styled(
                format!(
                    " {} · {following} ",
                    LogSummary {
                        lines: summary.lines.max(open.log.len() as u64),
                        ..summary.clone()
                    }
                    .description()
                ),
                theme.dim(),
            )
            .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = usize::from(inner.height);
    let end = open.log.len().saturating_sub(open.scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line<'_>> = open
        .log
        .range(start..end)
        .map(|line| {
            let line = without_timestamp(line);
            let style = if line.starts_with("##[group]") {
                bold()
            } else if line.starts_with("##[error]") {
                theme.tone(Tone::Bad)
            } else if line.starts_with("##[") {
                theme.dim()
            } else {
                Style::new()
            };
            Line::styled(line.to_owned(), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The line without the runner's `2026-10-22T22:27:32.7093602Z ` prefix, which takes a quarter of a narrow screen.
pub(super) fn without_timestamp(line: &str) -> &str {
    match line.split_once(' ') {
        Some((stamp, rest))
            if stamp.len() >= 20
                && stamp.ends_with('Z')
                && stamp.as_bytes().get(10) == Some(&b'T')
                && stamp.starts_with(|c: char| c.is_ascii_digit()) =>
        {
            rest
        }
        _ => line,
    }
}
