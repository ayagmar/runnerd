//! What needs attention, as core lists and words it (ADR 0132): the header's count and the overview's list, each item
//! its title, what clears it and how old its evidence is.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use runnerd_core::api::{self, Fix};
use unicode_width::UnicodeWidthStr;

use super::{Theme, fit, length, list_block, selected};
use crate::tui::app::{App, Pane};
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

/// The glyph before each item and the count, in the item's tone.
const MARK: &str = "▲";

/// The header's count of what needs attention, in the tone of its most urgent item; none when nothing does, or
/// before the first read.
pub(super) fn count(app: &App, theme: Theme) -> Option<Span<'static>> {
    let attention = app.attention.get()?;
    let count = attention.items.len() as u64 + attention.omitted;
    let first = attention.items.first()?;
    Some(Span::styled(
        format!("{MARK} {count}"),
        theme.of(first.tone),
    ))
}

/// The rows the list wants: its items, at most `most`, and a line for the rest; one for a daemon without the list;
/// none when nothing needs attention.
pub(super) fn wanted(app: &App, most: usize) -> u16 {
    match &app.attention {
        Reading::Read(attention) if attention.items.is_empty() => 0,
        Reading::Read(attention) => {
            let shown = attention.items.len().min(most);
            length(shown + 1 + usize::from(attention.more(shown).is_some()))
        }
        Reading::Unread(Unread::Unsupported) => 1,
        Reading::Unread(_) => 0,
    }
}

/// The items, most urgent first: the mark and title, then what clears it, dim, and how old its evidence is on the
/// right; then how many more there are.
pub(super) fn attention(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let attention = match &app.attention {
        Reading::Read(attention) => attention,
        Reading::Unread(Unread::Unsupported) => {
            frame.render_widget(
                Line::styled(
                    format!("{MARK} attention: {}", api::NOT_SUPPORTED),
                    theme.dim(),
                ),
                area,
            );
            return;
        }
        Reading::Unread(_) => return,
    };
    let focused = app.pane == Pane::Attention;
    let block = list_block("Attention", focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let more_row = attention.items.len() > usize::from(inner.height);
    let shown = usize::from(inner.height)
        .saturating_sub(usize::from(more_row))
        .min(attention.items.len());
    // The selected item stays in sight.
    let first = if focused {
        app.notice.saturating_sub(shown.saturating_sub(1))
    } else {
        0
    };
    let width = usize::from(inner.width);
    let mut lines = Vec::new();
    for (at, item) in attention.items.iter().enumerate().skip(first).take(shown) {
        let since = item
            .since
            .map(|since| format!("  {}", api::ago(since, now)))
            .unwrap_or_default();
        let fix = item.fix.as_ref().map(Fix::line).unwrap_or_default();
        let head = 2;
        let title_width = item.title.width().min(width.saturating_sub(head));
        let rest = width.saturating_sub(head + title_width);
        // What clears it and its age, as room allows: the age first goes, then the fix is cut.
        let since = if since.width() + 2 <= rest {
            since
        } else {
            String::new()
        };
        let fix_room = rest.saturating_sub(since.width() + 2);
        let fix = if fix.is_empty() || fix_room < 8 {
            String::new()
        } else {
            format!("  {}", fit(&fix, fix_room).trim_end())
        };
        let pad = width.saturating_sub(head + title_width + fix.width() + since.width());
        let line = Line::from(vec![
            Span::styled(format!("{MARK} "), theme.of(item.tone)),
            Span::raw(fit(&item.title, title_width)),
            Span::styled(fix, theme.dim()),
            Span::raw(" ".repeat(pad)),
            Span::styled(since, theme.dim()),
        ]);
        lines.push(if focused && at == app.notice {
            line.style(selected())
        } else {
            line
        });
    }
    if let Some(more) = attention.more(first + shown).filter(|_| more_row) {
        lines.push(Line::styled(format!("  {more}"), theme.dim()));
    }
    let [list, _] =
        Layout::vertical([Constraint::Length(length(shown)), Constraint::Min(0)]).areas(inner);
    hits.rows(list, List::Attention, first, shown);
    frame.render_widget(ratatui::widgets::Paragraph::new(lines), inner);
}
