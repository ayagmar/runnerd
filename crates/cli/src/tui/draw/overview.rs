//! The overview: the headline figures, what needs attention as one line, the budget strip, a lane per slot in the
//! strip's order, the queue with why each job waits (the selected job's reason whole under it while it is focused), the
//! repositories and the latest jobs, and the slice's charts when there is room.
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Table, TableState};
use runnerd_core::api::{
    self, Activity, HealthKind, JobSummary, OUTCOME_MAX, QueuedJob, RepositoryStatus, WaitKind,
};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::attention::{self, attention};
use super::budget::{self, budget};
use super::job::drop_least;
use super::slice;
use super::slots::lanes;
use super::{bold, fit, heading, length, list_block, picked, row_of, spark};
use crate::human::{self, ago, duration};
use crate::tui::app::{App, Pane};
use crate::tui::mouse::{Hits, List};

/// The fewest rows the repositories and latest jobs keep: a title and four jobs.
pub(super) const LISTS_MIN: u16 = 5;

/// The rows the repositories and latest jobs keep before the charts take theirs: a title and seven jobs.
const LISTS_FAIR: u16 = 8;

/// The fewest rows a chart is drawn in: its border and two rows of bars.
const CHARTS_MIN: u16 = 4;

/// The share of the overview the attention list takes at most while it is open.
const ATTENTION_SHARE: u16 = 2;

/// The most rows the selected queued job's reason takes under the queue; its page has room for more.
const REASON_MOST: usize = 4;

/// The queue's rows a focused queue keeps beside its heading and the selected job's reason.
const QUEUE_FOCUSED_ROWS: usize = 3;

/// Top to bottom: the four headline figures, what needs attention as one line (the whole list while it is focused),
/// the budget strip, a lane per slot in the strip's order, the queue, then the repositories and latest jobs; the
/// slice's charts take what the lists do not need, as tall as their samples reach, and are the first to go.
pub(super) fn overview(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let slots = length(app.slots().count());
    let health_height: u16 = if area.height >= 20 { 2 } else { 1 };
    let bar_rows: u16 = if area.height >= 36 { 2 } else { 1 };
    let budget_height = budget::height(bar_rows);
    let attention_open = app.pane == Pane::Attention;
    let attention_height = if attention_open {
        attention::open_height(app, area.width, area.height / ATTENTION_SHARE)
    } else {
        attention::strip_height(app)
    };
    let rest = area
        .height
        .saturating_sub(health_height + attention_height + budget_height);
    let QueueRows {
        wanted: queue_wanted,
        kept: queue_kept,
    } = QueueRows::of(app, now, area.width);
    let lanes_height = slots.min(rest.saturating_sub(LISTS_MIN.min(rest / 2) + queue_kept));
    let left = rest.saturating_sub(lanes_height);
    let queue_height = if queue_wanted == 0 {
        0
    } else {
        queue_wanted
            .min(left.saturating_sub(LISTS_MIN))
            .max(left.min(queue_kept.max(2)))
    };
    let left = left.saturating_sub(queue_height);
    let side_by_side = area.width >= 100;
    // The charts keep their height while the lists keep a fair share; the lists take every other row.
    let lists_kept = lists_height(app, side_by_side).clamp(LISTS_MIN, LISTS_FAIR);
    let charts_height = slice::height(app).min(left.saturating_sub(lists_kept));
    let charts_height = if charts_height >= CHARTS_MIN {
        charts_height
    } else {
        0
    };
    let [
        health_area,
        attention_area,
        budget_area,
        lanes_area,
        queue_area,
        lists_area,
        charts_area,
    ] = Layout::vertical([
        Constraint::Length(health_height),
        Constraint::Length(attention_height),
        Constraint::Length(budget_height),
        Constraint::Length(lanes_height),
        Constraint::Length(queue_height),
        Constraint::Min(0),
        Constraint::Length(charts_height),
    ])
    .areas(area);
    if health_height > 1 {
        tiles(frame, health_area, app, now, theme);
    } else {
        health(frame, health_area, app, now, theme);
    }
    if attention_open {
        attention(frame, attention_area, app, now, theme, hits);
    } else if attention_height > 0 {
        attention::strip(frame, attention_area, app, theme, hits);
    }
    budget(frame, budget_area, app, theme);
    lanes(frame, lanes_area, app, now, theme, hits);
    if queue_height > 0 {
        queue(frame, queue_area, app, now, theme, hits);
    }
    if charts_height > 0 {
        slice::slice(frame, charts_area, app, theme);
    }
    let [repositories_area, latest_area] = if !side_by_side && lists_area.height < 4 {
        // Too short for both lists one above the other: the focused one alone, so a focused list is always drawn.
        if app.pane == Pane::Latest {
            [Rect::default(), lists_area]
        } else {
            [lists_area, Rect::default()]
        }
    } else if side_by_side {
        let width = repositories_width(app, lists_area.width * 2 / 5);
        Layout::horizontal([Constraint::Length(width), Constraint::Min(0)]).areas(lists_area)
    } else {
        // Stacked, the repositories take the rows they need, at most half unless the latest jobs need less.
        let latest = length(app.live.recent.len() + 1).min(LISTS_MIN);
        let height = length(app.live.status.repositories.len() + 1)
            .min((lists_area.height / 2).max(lists_area.height.saturating_sub(latest)))
            .max(lists_area.height.min(2));
        Layout::vertical([Constraint::Length(height), Constraint::Min(0)]).areas(lists_area)
    };
    repositories(frame, repositories_area, app, theme, hits);
    latest(frame, latest_area, app, now, theme, hits);
}

/// The four headline figures as tiles across the width: each one's label above, its value in bold with what it is
/// of and its notes beside it, as many as fit whole, and the hours' longest waits beside the p95 where there is room.
pub(super) fn tiles(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    const SPARK_LEAST: usize = 6;
    let figures = app.live.status.health(now).figures(app.activity.as_ref());
    let count = length(figures.len().max(1));
    let [labels, values] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    let columns = Layout::horizontal(vec![Constraint::Ratio(1, u32::from(count)); figures.len()])
        .split(labels);
    let hours = app
        .activity
        .as_ref()
        .map(Activity::wait_hours)
        .unwrap_or_default();
    for (figure, column) in figures.into_iter().zip(columns.iter()) {
        // A cell between tiles.
        let room = usize::from(column.width).saturating_sub(1);
        frame.render_widget(
            Line::styled(fit(figure.label, room).trim_end().to_owned(), theme.dim()),
            *column,
        );
        let value = match figure.tone {
            api::Tone::Warn => theme.wait().add_modifier(Modifier::BOLD),
            api::Tone::Neutral | api::Tone::Quiet | api::Tone::Good => bold(),
            tone @ api::Tone::Bad => theme.tone(tone.into()).add_modifier(Modifier::BOLD),
        };
        let mut spans = vec![Span::styled(figure.value, value)];
        let mut used = Line::from(spans.clone()).width();
        if let Some(of) = figure.of {
            let of = format!(" {of}");
            if used + of.width() <= room {
                used += of.width();
                spans.push(Span::raw(of));
            }
        }
        if figure.kind == HealthKind::Wait && hours.iter().any(|wait| *wait > 0) {
            let bars = room.saturating_sub(used + 1).min(hours.len());
            if bars >= SPARK_LEAST {
                let bars = resample(&hours, bars);
                let longest = bars.iter().copied().max().unwrap_or(0);
                used += 1 + bars.len();
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    spark(bars.into_iter(), longest, usize::MAX),
                    theme.wait(),
                ));
            }
        }
        for note in figure.notes {
            let note = format!(", {note}");
            if used + note.width() > room {
                break;
            }
            used += note.width();
            spans.push(Span::styled(note, theme.dim()));
        }
        frame.render_widget(
            Line::from(spans),
            Rect {
                y: values.y,
                ..*column
            },
        );
    }
}

/// The rows the repositories and latest jobs would fill, side by side or one above the other.
pub(super) fn lists_height(app: &App, side_by_side: bool) -> u16 {
    let repositories = app.live.status.repositories.len() + 1;
    let latest = app.live.recent.len() + 1;
    length(if side_by_side {
        repositories.max(latest)
    } else {
        repositories + latest
    })
}

/// The figures that say whether CI keeps up, as the dashboard opens with them: slots running, jobs waiting and the
/// oldest's wait, then over the last 24 hours the p95 wait with the longest wait of each hour, and the failures. What
/// does not fit goes whole, least telling first: the starting and warm runners, the finished count, the hours and the
/// oldest wait. What the history says is in the attention list and the insights view, not here.
pub(super) fn health(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    const SPARK_LEAST: usize = 8;
    let mut items = health_items(app, now, theme);
    let width = usize::from(area.width);
    let used = |items: &[(u8, Option<Vec<Span<'static>>>)]| -> usize {
        items
            .iter()
            .map(|(_, spans)| {
                spans
                    .as_ref()
                    .map_or(1 + SPARK_LEAST, |spans| Line::from(spans.clone()).width())
            })
            .sum()
    };
    drop_least(&mut items, 1, |items| used(items) <= width);
    // On the narrowest line the last figures go whole too, rather than be cut.
    drop_least(&mut items, 0, |items| used(items) <= width);
    // What the hours' bars may take: the line's room less the other items and a space.
    let room = width
        .saturating_sub(used(&items).saturating_sub(1 + SPARK_LEAST))
        .saturating_sub(1);
    let hours = app
        .activity
        .as_ref()
        .map(Activity::wait_hours)
        .unwrap_or_default();
    let mut spans = Vec::new();
    for (_, item) in items {
        if let Some(item) = item {
            spans.extend(item);
        } else {
            let bars = resample(&hours, room.min(hours.len()));
            let longest = bars.iter().copied().max().unwrap_or(0);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                spark(bars.into_iter(), longest, usize::MAX),
                theme.wait(),
            ));
        }
    }
    frame.render_widget(Line::from(spans), area);
}

/// The health line's figures, each with its rank for [`drop_least`]; `None` marks where the hours' bars go. The
/// words are core's: a figure's label, value and notes, as the dashboard's tiles show them.
pub(super) fn health_items(
    app: &App,
    now: u64,
    theme: Theme,
) -> Vec<(u8, Option<Vec<Span<'static>>>)> {
    let sep = || Span::styled(" · ", theme.dim());
    let mut items: Vec<(u8, Option<Vec<Span<'static>>>)> = Vec::new();
    for (index, figure) in app
        .live
        .status
        .health(now)
        .figures(app.activity.as_ref())
        .into_iter()
        .enumerate()
    {
        let value = match figure.tone {
            api::Tone::Warn => theme.wait().add_modifier(Modifier::BOLD),
            api::Tone::Neutral | api::Tone::Quiet | api::Tone::Good => bold(),
            tone @ api::Tone::Bad => theme.tone(tone.into()),
        };
        let mut head = Vec::new();
        if index > 0 {
            head.push(sep());
        }
        head.push(Span::styled(format!("{}: ", figure.label), theme.dim()));
        head.push(Span::styled(figure.value, value));
        // What the running figure is of is part of it; the failed figure's total is the first detail to go.
        let of_rank = if figure.label == "Running" { 0 } else { 3 };
        let of = figure
            .of
            .map(|of| (of_rank, Some(vec![Span::raw(format!(" {of}"))])));
        items.push((0, Some(head)));
        items.extend(of);
        if figure.label == "Wait p95, 24 h"
            && app
                .activity
                .as_ref()
                .is_some_and(|day| day.wait_hours().iter().any(|wait| *wait > 0))
        {
            items.push((2, None));
        }
        let rank = match figure.label {
            "Waiting" => 1,
            "Wait p95, 24 h" => 6,
            _ => 4,
        };
        for note in figure.notes {
            items.push((
                rank,
                Some(vec![Span::styled(format!(", {note}"), theme.dim())]),
            ));
        }
    }
    items
}

/// `values` in `width` buckets, each the largest of the values it covers, in order.
pub(super) fn resample(values: &[u64], width: usize) -> Vec<u64> {
    if width == 0 {
        return Vec::new();
    }
    (0..width)
        .map(|bucket| {
            let start = bucket * values.len() / width;
            let end = ((bucket + 1) * values.len() / width).max(start + 1);
            values[start..end.min(values.len())]
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
        })
        .collect()
}

/// How a reason is drawn: what the owner can act on stands out, what clears by itself is quiet.
pub(super) fn reason_style(kind: WaitKind, theme: Theme) -> Style {
    match kind {
        WaitKind::Act => theme.wait().add_modifier(Modifier::BOLD),
        WaitKind::Soon | WaitKind::Other => theme.dim(),
        WaitKind::Full => theme.wait(),
    }
}

/// Why `job` waits, whole, in the daemon's words as the dashboard shows them; nothing from a daemon that does not
/// say.
pub(super) fn why(app: &App, job: &QueuedJob, now: u64, theme: Theme) -> Option<Span<'static>> {
    job.waiting.as_ref().map(|waiting| {
        Span::styled(
            waiting.reason(job, &app.live.status, now),
            reason_style(waiting.kind(), theme),
        )
    })
}

/// Why `job` waits in `width` cells, cut when it is longer.
fn reason(app: &App, job: &QueuedJob, now: u64, width: usize, theme: Theme) -> Span<'static> {
    why(app, job, now, theme).map_or_else(Span::default, |why| {
        Span::styled(fit(&why.content, width).trim_end().to_owned(), why.style)
    })
}

/// What a reason cut short ends in.
const CUT: &str = " …";

/// `text` in lines of at most `width` cells, broken between words, and a word wider than a line broken across lines
/// between graphemes where it reaches the edge: unlike [`super::wrap`], every character is kept, as a reason is read
/// whole.
pub(super) fn wrap_whole(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() <= width {
            line.push(' ');
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // Broken between graphemes, as the terminal draws them: an emoji and the selector after it, or a letter
        // and its accent, move to the next line together. The width is measured on the whole line, as a character
        // and a selector take more cells together than apart; a lone grapheme wider than the line stays, as nothing
        // narrower holds it.
        for grapheme in Span::raw(word).styled_graphemes(Style::default()) {
            if !line.is_empty() && (line.clone() + grapheme.symbol).width() > width {
                lines.push(std::mem::take(&mut line));
            }
            line.push_str(grapheme.symbol);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `why` wrapped under what it belongs to, `width` cells wide and indented by two, every character kept, in at most
/// `most` lines: a reason longer than that ends in ` …`, which its lines leave room for.
pub(super) fn reason_lines(
    why: &Span<'_>,
    width: usize,
    most: usize,
    theme: Theme,
) -> Vec<Line<'static>> {
    let indented = |text: String| Line::styled(format!("  {text}"), why.style);
    let whole = wrap_whole(&why.content, width.saturating_sub(2));
    if whole.len() <= most {
        return whole.into_iter().map(indented).collect();
    }
    let mut lines: Vec<Line<'static>> =
        wrap_whole(&why.content, width.saturating_sub(2 + CUT.width()))
            .into_iter()
            .take(most)
            .map(indented)
            .collect();
    if let Some(last) = lines.last_mut() {
        last.spans.push(Span::styled(CUT, theme.dim()));
    }
    lines
}

/// The rows the overview's queue wants.
struct QueueRows {
    /// Its heading, a row per job and, while it is focused, the selected job's reason under them; 0 when nothing waits.
    wanted: u16,
    /// What it keeps before the lanes take their rows: while it is focused, its heading, a few rows round the selected
    /// job and that job's reason, so they are in sight at 80 by 24 however many slots there are; else 0.
    kept: u16,
}

impl QueueRows {
    fn of(app: &App, now: u64, width: u16) -> QueueRows {
        let queued = app.live.status.queued.len();
        if queued == 0 {
            return QueueRows { wanted: 0, kept: 0 };
        }
        if app.pane != Pane::Queue {
            return QueueRows {
                wanted: length(queued + 1),
                kept: 0,
            };
        }
        let columns = QueueColumns::of(app, now, usize::from(width));
        let detail = length(detail(app, &columns, now, REASON_MOST, Theme { colour: false }).len());
        let wanted = length(queued + 1).saturating_add(detail);
        QueueRows {
            wanted,
            kept: wanted.min(1 + length(queued.min(QUEUE_FOCUSED_ROWS)) + detail),
        }
    }
}

/// The queue's cells: each job's wait, name and place, and the widths of their columns; 0 for one left out.
struct QueueColumns {
    waits: Vec<String>,
    names: Vec<String>,
    places: Vec<String>,
    /// The whole queue's.
    width: usize,
    wait: usize,
    name: usize,
    place: usize,
    reason: usize,
}

impl QueueColumns {
    /// The wait as wide as the widest, the name at most three tenths of `width`, the repository (and pool) from 120
    /// columns; the reason takes the rest.
    fn of(app: &App, now: u64, width: usize) -> QueueColumns {
        let queued = &app.live.status.queued;
        let waits: Vec<String> = queued
            .iter()
            .map(|job| match job.queued_at {
                0 => "–".to_owned(),
                at => duration(now.saturating_sub(at)),
            })
            .collect();
        let wait = waits.iter().map(|wait| wait.width()).max().unwrap_or(0);
        let names: Vec<String> = queued.iter().map(|job| job.title().to_string()).collect();
        let name = names
            .iter()
            .map(|name| name.width())
            .max()
            .unwrap_or(0)
            .min((width * 3 / 10).max(12));
        let places: Vec<String> = queued
            .iter()
            .map(|job| job.place(&app.live.status))
            .collect();
        let place = if width >= 120 {
            places
                .iter()
                .map(|place| place.width())
                .max()
                .unwrap_or(0)
                .min(32)
        } else {
            0
        };
        // A cell between columns.
        let reason =
            width.saturating_sub(wait + 1 + name + 1 + if place > 0 { place + 1 } else { 0 });
        QueueColumns {
            waits,
            names,
            places,
            width,
            wait,
            name,
            place,
            reason,
        }
    }
}

/// While the queue is focused, the selected job's reason whole under it when its row cuts it, in at most `most`
/// lines; nothing otherwise.
fn detail(
    app: &App,
    columns: &QueueColumns,
    now: u64,
    most: usize,
    theme: Theme,
) -> Vec<Line<'static>> {
    app.live
        .status
        .queued
        .get(app.queued)
        .filter(|_| app.pane == Pane::Queue)
        .and_then(|job| why(app, job, now, theme))
        .filter(|why| why.width() > columns.reason)
        .map_or_else(Vec::new, |why| {
            reason_lines(&why, columns.width, most, theme)
        })
}

/// Every queued job, oldest first: how long it has waited, its name, its repository (and pool) where there is room,
/// and why it waits, which takes the rest of the line; while the queue is focused, the selected job's reason whole
/// under the rows when its row cuts it.
pub(super) fn queue(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let focused = app.pane == Pane::Queue;
    let queued = &app.live.status.queued;
    let columns = QueueColumns::of(app, now, usize::from(area.width));
    // The heading and the selected job's row come first; the job's page shows the rest of a longer reason.
    let most = REASON_MOST.min(usize::from(area.height.saturating_sub(2)));
    let detail = detail(app, &columns, now, most, theme);
    let detail_height = length(detail.len());
    let [title, table_area, detail_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(detail_height),
    ])
    .areas(area);
    let QueueColumns {
        waits,
        names,
        places,
        wait: wait_width,
        width: _,
        name: name_width,
        place: repository_width,
        reason: reason_width,
    } = columns;
    let rows = queued
        .iter()
        .zip(waits)
        .zip(names)
        .zip(places)
        .enumerate()
        .map(|(at, (((job, wait), name), place))| {
            let mut cells = vec![
                Line::from(Span::styled(wait, theme.wait())).right_aligned(),
                Line::from(fit(&name, name_width)),
            ];
            if repository_width > 0 {
                cells.push(Line::from(fit(&place, repository_width)));
            }
            cells.push(Line::from(reason(app, job, now, reason_width, theme)));
            row_of(cells, focused && at == app.queued)
        });
    let mut widths = vec![
        Constraint::Length(length(wait_width)),
        Constraint::Length(length(name_width)),
    ];
    if repository_width > 0 {
        widths.push(Constraint::Length(length(repository_width)));
    }
    widths.push(Constraint::Fill(1));
    let table = Table::new(rows, widths).row_highlight_style(super::selected());
    let mut state = TableState::default().with_selected(focused.then_some(app.queued));
    frame.render_stateful_widget(table, table_area, &mut state);
    frame.render_widget(Paragraph::new(detail), detail_area);
    // The rows scrolled past and those under the table, as the table drew them.
    let above = state.offset().min(queued.len());
    let below = queued
        .len()
        .saturating_sub(above + usize::from(table_area.height));
    let mut note = vec!["oldest first".to_owned()];
    if above > 0 {
        note.push(format!("{above} above"));
    }
    if below > 0 {
        note.push(format!("{below} more below"));
    }
    heading(frame, title, "Waiting", note.join("; "), theme);
    hits.rows(table_area, List::Queue, above, queued.len() - above);
}

/// What the repositories pane needs to show each repository whole, with its profiles when that fits in `most`
/// cells, and a cell before the next pane: the rest goes to the latest jobs.
pub(super) fn repositories_width(app: &App, most: u16) -> u16 {
    let widest = |profiles: bool| {
        let widest = app
            .live
            .status
            .repositories
            .iter()
            .map(|repository| {
                // `■ `, the name, `  profile`, `  N queued`, `  paused`.
                2 + repository.name.width()
                    + if profiles {
                        2 + repository.profiles().width()
                    } else {
                        0
                    }
                    + format!("  {} queued", repository.queued_jobs).width()
                    + if repository.paused { 8 } else { 0 }
            })
            .max()
            .unwrap_or(0);
        length(widest + 1).max(24)
    };
    Some(widest(true))
        .filter(|width| *width <= most)
        .unwrap_or_else(|| widest(false))
        .min(most)
}

pub(super) fn repositories(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    theme: Theme,
    hits: &mut Hits,
) {
    let focused = app.pane == Pane::Repositories;
    let repositories = &app.live.status.repositories;
    // A cell stays free before the latest jobs: the profiles, which the square's colour also shows for the first
    // pool, go from every line first, then a name is cut.
    let width = usize::from(area.width).saturating_sub(1);
    let spans = |repository: &RepositoryStatus, profile: Option<&str>, name_width: usize| {
        let mut spans = vec![
            Span::styled(
                "■ ",
                theme.profile(app.profile_index(repository.first_profile())),
            ),
            Span::styled(
                fit(&repository.name, name_width).trim_end().to_owned(),
                bold(),
            ),
        ];
        if let Some(profile) = profile {
            spans.push(Span::styled(format!("  {profile}"), theme.dim()));
        }
        spans.push(Span::raw(format!("  {} queued", repository.queued_jobs)));
        if repository.paused {
            spans.push(Span::styled(
                "  paused",
                theme.wait().add_modifier(Modifier::BOLD),
            ));
        }
        spans
    };
    let with_profile = repositories.iter().all(|repository| {
        let name_width = repository.name.width();
        Line::from(spans(repository, Some(&repository.profiles()), name_width)).width() <= width
    });
    let lines: Vec<Line<'_>> = repositories
        .iter()
        .enumerate()
        .map(|(position, repository)| {
            let profiles = repository.profiles();
            let profile = with_profile.then_some(profiles.as_str());
            let others = Line::from(spans(repository, profile, 0)).width();
            let spans = spans(repository, profile, width.saturating_sub(others));
            let line = Line::from(spans);
            if focused && position == app.repository {
                picked(line)
            } else {
                line
            }
        })
        .collect();
    let block = list_block("Repositories", focused, theme);
    let hidden = repositories
        .len()
        .saturating_sub(usize::from(block.inner(area).height));
    let block = more_below(block, hidden, theme);
    hits.rows(block.inner(area), List::Repositories, 0, repositories.len());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// A list's block saying how many of its rows do not fit, as the queue's heading does.
fn more_below(block: Block<'_>, hidden: usize, theme: Theme) -> Block<'_> {
    if hidden == 0 {
        return block;
    }
    block.title_top(Line::styled(format!(" {hidden} more below "), theme.dim()).right_aligned())
}

/// An outcome as an upper-case badge, in at most `OUTCOME_MAX` cells.
pub(super) fn badge(job: &JobSummary, theme: Theme) -> Span<'static> {
    let (word, tone) = human::outcome(job);
    Span::styled(
        badge_text(&word),
        theme.tone(tone).add_modifier(Modifier::BOLD),
    )
}

/// `word` in upper case, cut to `OUTCOME_MAX` cells: upper case may be wider than the label core bounds, as `ß`
/// becomes `SS`.
fn badge_text(word: &str) -> String {
    let upper = word.to_uppercase();
    if upper.width() <= OUTCOME_MAX {
        upper
    } else {
        fit(&upper, OUTCOME_MAX)
    }
}

/// The cells the widest of `jobs`' badges takes, so a column of them never cuts an outcome such as `startup
/// failure`, and never takes more than `OUTCOME_MAX`.
pub(super) fn badge_width<'job>(jobs: impl IntoIterator<Item = &'job JobSummary>) -> usize {
    jobs.into_iter()
        .map(|job| badge_text(&job.outcome().label).width())
        .max()
        .unwrap_or(0)
}

/// How long ago GitHub queued `job`.
fn queued_ago(job: &JobSummary, now: u64) -> String {
    ago(job.timeline.queued_at, now)
}

/// The latest finished jobs, newest first: the outcome, the job, its repository where there is room, from 110 columns
/// how long it waited and from 140 its branch, how long it ran and when it was queued.
pub(super) fn latest(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let focused = app.pane == Pane::Latest;
    let jobs = &app.live.recent;
    let block = list_block("Latest jobs", focused, theme);
    let inner = block.inner(area);
    let hidden = jobs.len().saturating_sub(usize::from(inner.height));
    frame.render_widget(more_below(block, hidden, theme), area);
    let widths = LatestWidths::of(jobs, inner.width, now);
    let rows = jobs.iter().enumerate().map(|(at, job)| {
        let mut cells = vec![
            Line::from(badge(job, theme)),
            Line::from(fit(&job.title().to_string(), widths.title)),
        ];
        if widths.repository > 0 {
            cells.push(Line::from(fit(&job.repository, widths.repository)));
        }
        if widths.branch > 0 {
            cells.push(Line::from(Span::styled(
                fit(job.branch.as_deref().unwrap_or_default(), widths.branch),
                theme.dim(),
            )));
        }
        if widths.wait > 0 {
            let wait = job.waited(now).map_or_else(String::new, duration);
            cells.push(Line::from(Span::styled(wait, theme.wait())).right_aligned());
        }
        let ran = job.ran(now).map_or_else(String::new, duration);
        cells.extend([
            Line::from(ran).right_aligned(),
            Line::from(Span::styled(queued_ago(job, now), theme.dim())).right_aligned(),
        ]);
        row_of(cells, focused && at == app.latest)
    });
    let table = Table::new(rows, widths.constraints()).row_highlight_style(super::selected());
    let mut state = TableState::default().with_selected(focused.then_some(app.latest));
    frame.render_stateful_widget(table, inner, &mut state);
    hits.rows(
        inner,
        List::Latest,
        state.offset(),
        jobs.len() - state.offset().min(jobs.len()),
    );
}

/// The latest jobs' columns, in cells; 0 for a column left out.
struct LatestWidths {
    outcome: usize,
    title: usize,
    repository: usize,
    branch: usize,
    wait: usize,
    ran: usize,
    ago: usize,
}

impl LatestWidths {
    /// The times as wide as the widest of them; the job and its repository take what is left, the job first. The
    /// waits from 110 columns, the branches from 140.
    fn of(jobs: &[JobSummary], width: u16, now: u64) -> LatestWidths {
        let widest = |widths: &mut dyn Iterator<Item = usize>| widths.max().unwrap_or(0);
        let ran = widest(
            &mut jobs
                .iter()
                .map(|job| job.ran(now).map_or_else(String::new, duration).width()),
        );
        let ago = widest(&mut jobs.iter().map(|job| queued_ago(job, now).width()));
        let wait = if width >= 110 {
            widest(
                &mut jobs
                    .iter()
                    .map(|job| job.waited(now).map_or_else(String::new, duration).width()),
            )
        } else {
            0
        };
        let branch = if width >= 140 {
            widest(
                &mut jobs
                    .iter()
                    .map(|job| job.branch.as_deref().unwrap_or_default().width()),
            )
            .min(20)
        } else {
            0
        };
        let optional: usize = [wait, branch]
            .iter()
            .filter(|width| **width > 0)
            .map(|width| width + 1)
            .sum();
        let outcome = badge_width(jobs);
        let rest = usize::from(width).saturating_sub(outcome + 1 + ran + 1 + ago + 1 + optional);
        let title = widest(&mut jobs.iter().map(|job| job.title().to_string().width()));
        let repository = widest(&mut jobs.iter().map(|job| job.repository.width()));
        // The repository shows from a dozen cells, cut if it must be.
        let (title, repository) = if rest >= title + 1 + repository.min(12) {
            (title, rest - title - 1)
        } else {
            (rest, 0)
        };
        LatestWidths {
            outcome,
            title,
            repository,
            branch,
            wait,
            ran,
            ago,
        }
    }

    /// The table's widths, without the columns left out.
    fn constraints(&self) -> Vec<Constraint> {
        let optional = [self.repository, self.branch, self.wait];
        [self.outcome, self.title]
            .into_iter()
            .chain(optional.into_iter().filter(|width| *width > 0))
            .chain([self.ran, self.ago])
            .map(|width| Constraint::Length(length(width)))
            .collect()
    }
}
