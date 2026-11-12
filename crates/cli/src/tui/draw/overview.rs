//! The overview: what needs attention, the health line, the budget, a lane per slot, the queue with why each job
//! waits, the repositories and the latest jobs, and the slice's charts when there is room.
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState};
use runnerd_core::api::{self, Activity, JobSummary, QueuedJob, RepositoryStatus, WaitKind};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::attention::{self, attention};
use super::budget::budget;
use super::job::drop_least;
use super::slice::slice;
use super::slots::lanes;
use super::{bold, fit, heading, length, list_block, selected, spark};
use crate::human::{self, ago, duration};
use crate::tui::app::{App, Pane};
use crate::tui::mouse::{Hits, List};

/// The fewest rows the repositories and latest jobs keep: a title and four jobs.
pub(super) const LISTS_MIN: u16 = 5;

/// The most items the attention list shows on the overview; the rest are counted.
const ATTENTION_SHOWN: usize = 5;

/// The rows the slice's charts take at most: they show half an hour, which more rows would not add to.
const CHARTS_MAX: u16 = 8;

/// The fewest rows a chart is drawn in: its border and three rows of bars.
const CHARTS_MIN: u16 = 5;

/// Top to bottom: health, what needs attention, budget, a lane per slot, the queue, then the repositories and
/// latest jobs, which take the rows left; the slice's charts take what the lists do not need, up to their size, and
/// are the first to go.
pub(super) fn overview(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let slots = length(app.slots().count());
    let queued = app.live.status.queued.len();
    let rest = area.height.saturating_sub(1 + 3);
    let queue_least = if queued == 0 {
        0
    } else {
        length(queued.min(3) + 1)
    };
    // The attention list gives way to the lanes and lists on a short terminal, down to one item.
    let attention_room = rest.saturating_sub(slots + queue_least + LISTS_MIN);
    let attention_height = attention::wanted(app, ATTENTION_SHOWN, area.width)
        .min(attention_room.max(
            attention::wanted(app, 1, area.width).min(rest.saturating_sub(slots + LISTS_MIN)),
        ));
    let rest = rest.saturating_sub(attention_height);
    let lanes_height = slots.min(rest.saturating_sub(LISTS_MIN.min(rest / 2)));
    let left = rest.saturating_sub(lanes_height);
    let queue_height = if queued == 0 {
        0
    } else {
        length(queued + 1)
            .min(left.saturating_sub(LISTS_MIN))
            .max(left.min(2))
    };
    let left = left.saturating_sub(queue_height);
    let side_by_side = area.width >= 100;
    let lists_wanted = lists_height(app, side_by_side).max(LISTS_MIN);
    let charts_height = left.saturating_sub(lists_wanted).min(CHARTS_MAX);
    let charts_height = if charts_height >= CHARTS_MIN && !app.host.is_empty() {
        charts_height
    } else {
        0
    };
    // Rows nothing else wants show more of what needs attention.
    let spare = left.saturating_sub(lists_wanted + charts_height);
    let attention_height = attention_height
        + spare
            .min(attention::wanted(app, usize::MAX, area.width).saturating_sub(attention_height));
    let [
        health_area,
        attention_area,
        budget_area,
        lanes_area,
        queue_area,
        lists_area,
        charts_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(attention_height),
        Constraint::Length(3),
        Constraint::Length(lanes_height),
        Constraint::Length(queue_height),
        Constraint::Min(0),
        Constraint::Length(charts_height),
    ])
    .areas(area);
    health(frame, health_area, app, now, theme);
    if attention_height > 0 {
        attention(frame, attention_area, app, now, theme, hits);
    }
    budget(frame, budget_area, app, theme);
    lanes(frame, lanes_area, app, now, theme, hits);
    if queue_height > 0 {
        queue(frame, queue_area, app, now, theme, hits);
    }
    if charts_height > 0 {
        slice(frame, charts_area, app, theme);
    }
    let [repositories_area, latest_area] = if side_by_side {
        let width = repositories_width(app).min(lists_area.width * 2 / 5);
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

/// Why `job` waits in `width` cells, in the daemon's words as the dashboard shows them; nothing from a daemon that
/// does not say.
pub(super) fn reason(
    app: &App,
    job: &QueuedJob,
    now: u64,
    width: usize,
    theme: Theme,
) -> Span<'static> {
    job.waiting.as_ref().map_or_else(Span::default, |waiting| {
        Span::styled(
            fit(&waiting.reason(job, &app.live.status, now), width)
                .trim_end()
                .to_owned(),
            reason_style(waiting.kind(), theme),
        )
    })
}

/// Every queued job, oldest first: how long it has waited, its name, its repository (and pool) where there is room,
/// and why it waits, which takes the rest of the line.
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
    let [title, table_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let hidden = queued.len().saturating_sub(usize::from(table_area.height));
    heading(
        frame,
        title,
        "Waiting",
        if hidden > 0 {
            format!("oldest first; {hidden} more below")
        } else {
            "oldest first".to_owned()
        },
        theme,
    );
    let width = usize::from(table_area.width);
    let waits: Vec<String> = queued
        .iter()
        .map(|job| match job.queued_at {
            0 => "–".to_owned(),
            at => duration(now.saturating_sub(at)),
        })
        .collect();
    let wait_width = waits.iter().map(|wait| wait.width()).max().unwrap_or(0);
    let names: Vec<String> = queued.iter().map(|job| job.title().to_string()).collect();
    let name_width = names
        .iter()
        .map(|name| name.width())
        .max()
        .unwrap_or(0)
        .min((width * 3 / 10).max(12));
    let places: Vec<String> = queued
        .iter()
        .map(|job| job.place(&app.live.status))
        .collect();
    let repository_width = if width >= 120 {
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
    let reason_width = width.saturating_sub(
        wait_width
            + 1
            + name_width
            + 1
            + if repository_width > 0 {
                repository_width + 1
            } else {
                0
            },
    );
    let rows =
        queued
            .iter()
            .zip(waits)
            .zip(names)
            .zip(places)
            .map(|(((job, wait), name), place)| {
                let mut cells = vec![
                    Cell::from(Line::from(Span::styled(wait, theme.wait())).right_aligned()),
                    Cell::from(fit(&name, name_width)),
                ];
                if repository_width > 0 {
                    cells.push(Cell::from(fit(&place, repository_width)));
                }
                cells.push(Cell::from(reason(app, job, now, reason_width, theme)));
                Row::new(cells)
            });
    let mut widths = vec![
        Constraint::Length(length(wait_width)),
        Constraint::Length(length(name_width)),
    ];
    if repository_width > 0 {
        widths.push(Constraint::Length(length(repository_width)));
    }
    widths.push(Constraint::Fill(1));
    let table = Table::new(rows, widths).row_highlight_style(selected());
    let mut state = TableState::default().with_selected(focused.then_some(app.queued));
    frame.render_stateful_widget(table, table_area, &mut state);
    hits.rows(
        table_area,
        List::Queue,
        state.offset(),
        queued.len() - state.offset().min(queued.len()),
    );
}

/// What the repositories pane needs to show each repository whole with its profile, and a cell before the next pane.
pub(super) fn repositories_width(app: &App) -> u16 {
    let widest = app
        .live
        .status
        .repositories
        .iter()
        .map(|repository| {
            // `■ `, the name, `  profile`, `  N queued`, `  paused`.
            2 + repository.name.width()
                + 2
                + repository.profiles().width()
                + format!("  {} queued", repository.queued_jobs).width()
                + if repository.paused { 8 } else { 0 }
        })
        .max()
        .unwrap_or(0);
    length(widest + 1).max(24)
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
                line.style(selected())
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

/// An outcome as an upper-case badge.
pub(super) fn badge(job: &JobSummary, theme: Theme) -> Span<'static> {
    let (word, tone) = human::outcome(job);
    Span::styled(
        word.to_uppercase(),
        theme.tone(tone).add_modifier(Modifier::BOLD),
    )
}

/// The cells the widest of `jobs`' badges takes, so a column of them never cuts an outcome such as `startup
/// failure`.
pub(super) fn badge_width<'job>(jobs: impl IntoIterator<Item = &'job JobSummary>) -> usize {
    jobs.into_iter()
        .map(|job| job.outcome().label.width())
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
    let rows = jobs.iter().map(|job| {
        let mut cells = vec![
            Cell::from(badge(job, theme)),
            Cell::from(fit(&job.title().to_string(), widths.title)),
        ];
        if widths.repository > 0 {
            cells.push(Cell::from(fit(&job.repository, widths.repository)));
        }
        if widths.branch > 0 {
            cells.push(Cell::from(Span::styled(
                fit(job.branch.as_deref().unwrap_or_default(), widths.branch),
                theme.dim(),
            )));
        }
        if widths.wait > 0 {
            let wait = job.waited(now).map_or_else(String::new, duration);
            cells.push(Cell::from(
                Line::from(Span::styled(wait, theme.wait())).right_aligned(),
            ));
        }
        let ran = job.ran(now).map_or_else(String::new, duration);
        cells.extend([
            Cell::from(Line::from(ran).right_aligned()),
            Cell::from(Line::from(Span::styled(queued_ago(job, now), theme.dim())).right_aligned()),
        ]);
        Row::new(cells)
    });
    let table = Table::new(rows, widths.constraints()).row_highlight_style(selected());
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
