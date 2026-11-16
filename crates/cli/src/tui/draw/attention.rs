//! What needs attention, as core lists and words it (ADR 0132): the header's count, the overview's one line, and the
//! whole list it opens, each item its title, what clears it and how old its evidence is.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use runnerd_core::api::{self, Fix};
use unicode_width::UnicodeWidthStr;

use super::{Theme, fit, length, list_block, wrap};
use crate::tui::app::{App, Pane};
use crate::tui::mouse::{Hits, List, Target};
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

/// The rows the overview's one line takes: one while something needs attention or the daemon cannot say, else none.
pub(super) fn strip_height(app: &App) -> u16 {
    match &app.attention {
        Reading::Read(attention) => u16::from(!attention.items.is_empty()),
        Reading::Unread(Unread::Unsupported) => 1,
        Reading::Unread(_) => 0,
    }
}

/// What needs attention as one line: the count and the most urgent item in its tone, how many more, and the key that
/// opens the whole list; a click opens it too. The item's title gives way first, as the list shows it whole.
pub(super) fn strip(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme, hits: &mut Hits) {
    let attention = match &app.attention {
        Reading::Read(attention) => attention,
        Reading::Unread(Unread::Unsupported) => {
            frame.render_widget(
                Line::styled(
                    format!("{MARK} {}", api::Feature::Attention.unsupported()),
                    theme.dim(),
                ),
                area,
            );
            return;
        }
        Reading::Unread(_) => return,
    };
    let Some(first) = attention.items.first() else {
        return;
    };
    let count = attention.items.len() as u64 + attention.omitted;
    let width = usize::from(area.width);
    let head = format!("{MARK} {count}  ");
    let more = attention
        .more(1)
        .map(|more| format!("  {more}"))
        .unwrap_or_default();
    let key = "  ! open";
    let room = width.saturating_sub(head.width() + more.width() + key.width());
    let title = fit(&first.title, room).trim_end().to_owned();
    let pad = width.saturating_sub(head.width() + title.width() + more.width() + key.width());
    frame.render_widget(
        Line::from(vec![
            Span::styled(head, theme.of(first.tone)),
            Span::raw(title),
            Span::styled(more, theme.dim()),
            Span::raw(" ".repeat(pad)),
            Span::styled(key, theme.dim()),
        ]),
        area,
    );
    hits.add(area, Target::Attention);
}

/// The rows the open list takes in `width` cells and at most `most` rows: every item whole when they fit, else a
/// line each with the selected one whole under them.
pub(super) fn open_height(app: &App, width: u16, most: u16) -> u16 {
    let whole = length(whole_lines(app, usize::from(width), Theme { colour: false }, 0).len() + 1);
    if whole <= most {
        whole
    } else {
        wanted(app, usize::MAX, width).min(most)
    }
}

/// Every item whole, each line with the item it belongs to: its mark and title wrapped, then what clears it, dim, on
/// the title's last line where it fits and under it where not, and how old its evidence is on the right; then how
/// many more there are.
fn whole_lines(
    app: &App,
    width: usize,
    theme: Theme,
    now: u64,
) -> Vec<(Option<usize>, Line<'static>)> {
    let Reading::Read(attention) = &app.attention else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for (at, item) in attention.items.iter().enumerate() {
        let picked = at == app.notice;
        let since = item
            .since
            .map(|since| format!("  {}", api::ago(since, now)))
            .unwrap_or_default();
        let fix = item.fix.as_ref().map(Fix::line).unwrap_or_default();
        let mut rows: Vec<Vec<Span<'static>>> = wrap(&item.title, width.saturating_sub(2))
            .into_iter()
            .enumerate()
            .map(|(row, text)| {
                let mark = if row == 0 {
                    Span::styled(format!("{MARK} "), theme.of(item.tone))
                } else {
                    Span::raw("  ")
                };
                vec![mark, Span::raw(text)]
            })
            .collect();
        let used = |spans: &[Span<'_>]| Line::from(spans.to_vec()).width();
        if !fix.is_empty() {
            let beside = rows
                .last()
                .is_some_and(|last| used(last) + 2 + fix.width() + since.width() <= width);
            if beside {
                if let Some(last) = rows.last_mut() {
                    last.push(Span::styled(format!("  {fix}"), theme.dim()));
                }
            } else {
                rows.extend(
                    wrap(&fix, width.saturating_sub(4))
                        .into_iter()
                        .map(|line| vec![Span::styled(format!("    {line}"), theme.dim())]),
                );
            }
        }
        if let Some(last) = rows.last_mut()
            && used(last) + since.width() <= width
        {
            let pad = width - used(last) - since.width();
            last.push(Span::raw(" ".repeat(pad)));
            last.push(Span::styled(since, theme.dim()));
        }
        for spans in rows {
            let line = Line::from(spans);
            lines.push((Some(at), if picked { super::picked(line) } else { line }));
        }
    }
    if let Some(more) = attention.more(attention.items.len()) {
        lines.push((None, Line::styled(format!("  {more}"), theme.dim())));
    }
    lines
}

/// The rows the list wants `width` cells wide: its items, at most `most`, and a line for the rest, and while it is
/// focused the selected item whole under them; one for a daemon without the list; none when nothing needs attention.
pub(super) fn wanted(app: &App, most: usize, width: u16) -> u16 {
    match &app.attention {
        Reading::Read(attention) if attention.items.is_empty() => 0,
        Reading::Read(attention) => {
            let shown = attention.items.len().min(most);
            length(
                shown
                    + 1
                    + usize::from(attention.more(shown).is_some())
                    + detail(app, usize::from(width), Theme { colour: false }).len(),
            )
        }
        Reading::Unread(Unread::Unsupported) => 1,
        Reading::Unread(_) => 0,
    }
}

/// The selected item's title, then what clears it, dim, each wrapped to `width`; nothing unless the list is focused.
fn detail(app: &App, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let Some(item) = app
        .notices()
        .get(app.notice)
        .filter(|_| app.pane == Pane::Attention)
    else {
        return Vec::new();
    };
    let mut lines: Vec<Line<'static>> = wrap(&item.title, width.saturating_sub(2))
        .into_iter()
        .map(|line| Line::raw(format!("  {line}")))
        .collect();
    if let Some(fix) = &item.fix {
        lines.extend(
            wrap(&fix.line(), width.saturating_sub(2))
                .into_iter()
                .map(|line| Line::styled(format!("  {line}"), theme.dim())),
        );
    }
    lines
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
                    format!("{MARK} {}", api::Feature::Attention.unsupported()),
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
    let whole = whole_lines(app, usize::from(inner.width), theme, now);
    if whole.len() <= usize::from(inner.height) {
        draw_whole(frame, inner, whole, hits);
        return;
    }
    // The selected item's title and fix whole, under the list, leaving it a row.
    let mut detail = detail(app, usize::from(inner.width), theme);
    // The list keeps the selected item's row, and the row counting the rest.
    let list_least = 1 + u16::from(attention.items.len() > 1);
    let detail_height = length(detail.len()).min(inner.height.saturating_sub(list_least));
    if detail.len() > usize::from(detail_height) {
        detail.truncate(usize::from(detail_height));
        if let Some(last) = detail.last_mut() {
            last.spans.push(Span::styled(" …", theme.dim()));
        }
    }
    let [inner, detail_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(detail_height)]).areas(inner);
    frame.render_widget(ratatui::widgets::Paragraph::new(detail), detail_area);
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
            super::picked(line)
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

/// Every item whole in `area`, each of its rows a hit for it.
fn draw_whole(
    frame: &mut Frame<'_>,
    area: Rect,
    whole: Vec<(Option<usize>, Line<'static>)>,
    hits: &mut Hits,
) {
    let mut lines = Vec::new();
    for (y, (item, line)) in (area.y..).zip(whole) {
        if let Some(item) = item {
            hits.add(
                Rect {
                    y,
                    height: 1,
                    ..area
                },
                Target::Row(List::Attention, item),
            );
        }
        lines.push(line);
    }
    frame.render_widget(ratatui::widgets::Paragraph::new(lines), area);
}
