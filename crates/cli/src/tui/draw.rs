//! What `runnerctl tui` draws: the overview (budget, slots, the slice, repositories, latest jobs), the history, the
//! records, sizing and one job (timeline, steps on one time axis, memory and CPU, its log). Reads `App` and the time
//! only; panes that do not fit a small terminal are left out or drawn one line each. Colours follow the web dashboard:
//! profiles in config order take its palette, mint first and pink second; without colour, glyphs and bold carry the
//! meaning.

use std::fmt::Write as _;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState, Wrap,
};
use runnerd_core::api::{
    self, Activity, AutoPlan, BudgetParts, ChartLine, ChartLineKind, ChartLines, HeldKind,
    HeldPart, Insights, Job, JobSummary, LogSummary, Phase, PhaseKind, ProfileSizing,
    ProfileStatus, QueuedJob, RecordNote, RecordState, Records, RepositoryStatus, RunComparison,
    Sample, Sizing, SlotStatus, Status, Step, WaitKind, cpus, in_cores, job_title,
};
use runnerd_core::config::ByteSize;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Connection, History, JobView, Outcome, Pane, Removal, View};
use crate::human::{self, Tone, bytes, cores_used, duration};
use crate::watch;

/// The web dashboard's profile colours as xterm-256 colours: mint, pink, lavender, yellow, blue, peach.
const PROFILES: [u8; 6] = [194, 224, 189, 229, 153, 223];
/// The dashboard's memory green, CPU lavender and waiting yellow.
const MEMORY: u8 = 114;
const CPU: u8 = 147;
const WAIT: u8 = 221;

/// How to style the screen: in colour, or with `NO_COLOR` in bold, dim and glyphs only.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    pub(crate) colour: bool,
}

impl Theme {
    fn fg(self, colour: Color) -> Style {
        if self.colour {
            Style::new().fg(colour)
        } else {
            Style::new()
        }
    }

    fn profile(self, index: Option<usize>) -> Style {
        index.map_or_else(
            || self.dim(),
            |index| self.fg(Color::Indexed(PROFILES[index % PROFILES.len()])),
        )
    }

    fn dim(self) -> Style {
        if self.colour {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    fn memory(self) -> Style {
        self.fg(Color::Indexed(MEMORY))
    }

    fn cpu(self) -> Style {
        self.fg(Color::Indexed(CPU))
    }

    fn wait(self) -> Style {
        self.fg(Color::Indexed(WAIT))
    }

    pub(super) fn tone(self, tone: Tone) -> Style {
        match tone {
            Tone::Plain => Style::new(),
            Tone::Bold => Style::new().add_modifier(Modifier::BOLD),
            Tone::Dim => self.dim(),
            Tone::Good => self.memory(),
            Tone::Bad => self.fg(Color::LightRed).add_modifier(Modifier::BOLD),
            Tone::Busy => self.cpu(),
        }
    }
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// The whole screen.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, now: u64, theme: Theme) {
    let [top, body, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    header(frame, top, app, theme);
    match app.view {
        View::Overview => overview(frame, body, app, now, theme),
        View::History => history(frame, body, app, now, theme),
        View::Records => records(frame, body, app, now, theme),
        View::Sizing => sizing(frame, body, app, now, theme),
        View::Job => {
            if let Some(open) = &app.job {
                job(frame, body, app, open, now, theme);
            }
        }
    }
    footer(frame, bottom, app, theme);
    if app.help {
        help(frame, body);
    }
}

fn header(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let mut spans = vec![Span::styled(" runnerd ", bold())];
    let version = Span::styled(format!("{}  ", app.live.status.version), theme.dim());
    for (key, name, view) in [
        ("1", "Overview", View::Overview),
        ("2", "History", View::History),
        ("3", "Records", View::Records),
        ("4", "Sizing", View::Sizing),
    ] {
        let style = if app.view == view {
            selected()
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!(" {key} {name} "), style));
    }
    if let Some(open) = &app.job {
        spans.push(Span::styled(
            format!(" job {} ", open.job.summary.id),
            selected(),
        ));
    }
    let mut right = Vec::new();
    if app.live.status.shutting_down {
        right.push(Span::styled("shutting down  ", theme.wait()));
    }
    match &app.connection {
        Connection::Live => right.push(Span::styled("■ live ", theme.memory())),
        Connection::Reconnecting(why) => right.push(Span::styled(
            format!("□ reconnecting: {why} "),
            theme.wait().add_modifier(Modifier::BOLD),
        )),
    }
    let right = Line::from(right);
    // The version goes first when the tabs and the connection do not fit beside it.
    if Line::from(spans.clone()).width() + version.width() + right.width()
        <= usize::from(area.width)
    {
        spans.insert(1, version);
    }
    frame.render_widget(Line::from(spans), area);
    frame.render_widget(right.alignment(Alignment::Right), area);
}

fn footer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let line = if let Some(refused) = &app.confirm {
        let ask = if refused.jobs == 1 {
            " y cancels the job's run and pauses, any other key keeps it running: ".to_owned()
        } else {
            // Jobs of one workflow share a run, so the count is of jobs.
            format!(
                " y cancels the runs of {} jobs and pauses, any other key keeps them running: ",
                refused.jobs
            )
        };
        let rest = usize::from(area.width).saturating_sub(ask.width());
        Line::from(vec![
            Span::styled(ask, bold()),
            Span::styled(fit(&refused.message, rest), theme.wait()),
        ])
    } else if let Some(removal) = app.removal {
        let ask = match removal {
            Removal::Job(job) => {
                format!(" y removes the record of job {job}, any other key keeps it")
            }
            Removal::All => " y removes every record, any other key keeps them".to_owned(),
        };
        Line::styled(ask, bold())
    } else if let Some(resizing) = &app.resize {
        Line::styled(
            format!(
                " y writes memory = \"{}\", slots = {} for {} into the daemon's config, any other key keeps it",
                ByteSize(resizing.resize.memory),
                resizing.resize.slots,
                resizing.profile
            ),
            bold(),
        )
    } else if let Some(flash) = &app.flash {
        let style = if flash.outcome == Outcome::Failed {
            theme.tone(Tone::Bad)
        } else {
            theme.memory()
        };
        Line::styled(format!(" {}", flash.text), style)
    } else {
        Line::styled(
            format!(
                " {}",
                footer_keys(app, usize::from(area.width).saturating_sub(1))
            ),
            theme.dim(),
        )
    };
    frame.render_widget(line, area);
}

/// The keys that matter on the view, those that do not fit left out from the least used: each view's own first,
/// then moving and opening, then help and quit, which stay.
fn footer_keys(app: &App, width: usize) -> String {
    let mut keys: Vec<(u8, &str)> = match app.view {
        View::Overview => {
            let mut keys = vec![
                (1, "tab pane"),
                (2, "j/k move"),
                (1, "enter open"),
                (1, "p/r pause/resume"),
                (3, "P/U all"),
            ];
            if app.pane == Pane::Slots {
                keys.push((2, "R reseed"));
            }
            keys
        }
        View::History => vec![
            (2, "j/k move"),
            (2, "enter open"),
            (1, "c outcome"),
            (1, "s repository"),
            (1, "w workflow"),
            (1, "] [ page"),
        ],
        View::Records => vec![
            (2, "j/k move"),
            (2, "enter open"),
            (1, "d remove"),
            (1, "D remove all"),
        ],
        View::Sizing => vec![(2, "j/k move"), (1, "a apply the advice")],
        View::Job => vec![
            (2, "j/k scroll"),
            (2, "G follow"),
            (1, "p/r pause/resume"),
            (3, "R reseed"),
            (1, "esc back"),
        ],
    };
    keys.extend([(4, "1-4 views"), (0, "? help"), (0, "q quit")]);
    let line = |keys: &[(u8, &str)]| {
        keys.iter()
            .map(|(_, key)| *key)
            .collect::<Vec<_>>()
            .join("  ")
    };
    drop_least(&mut keys, 1, |keys| line(keys).width() <= width);
    line(&keys)
}

fn help(frame: &mut Frame<'_>, area: Rect) {
    // At most 20, so it fits a terminal of 24 rows.
    const KEYS: [(&str, &str); 16] = [
        ("1 2 3 4", "overview, history, records, sizing; o and h too"),
        ("tab", "overview: slots, waiting, repositories, latest"),
        ("j k ↓ ↑", "move, or scroll the log"),
        ("PgDn PgUp", "move by ten"),
        ("enter esc", "open the job; back from it"),
        ("G g", "follow the log, or go to its start"),
        ("p r", "pause the repository, or resume it"),
        ("P U", "pause every repository, or resume them"),
        ("R", "reseed the profile's snapshot"),
        ("c s w", "history: cycle outcome, repository, workflow"),
        ("] [", "history: next and previous page"),
        ("d D", "records: remove the record, or every record"),
        ("a", "sizing: write the advice into the config"),
        ("?", "this help; any key closes it"),
        ("ctrl-z", "suspend; the shell's fg brings it back"),
        ("q ctrl-c", "quit"),
    ];
    let width = area.width.min(60);
    let height = area
        .height
        .min(u16::try_from(KEYS.len()).unwrap_or(u16::MAX) + 2);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let lines: Vec<Line<'_>> = KEYS
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<10}"), bold()),
                Span::raw(*what),
            ])
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        popup,
    );
}

/// A section's title on the left and a note on the right, on one line.
/// The note is cut to leave the title and two cells beside it.
fn heading(frame: &mut Frame<'_>, area: Rect, title: &str, note: String, theme: Theme) {
    styled_heading(frame, area, title, note, theme.dim());
}

/// `heading` with its note in `style`.
fn styled_heading(frame: &mut Frame<'_>, area: Rect, title: &str, note: String, style: Style) {
    frame.render_widget(Line::styled(title.to_owned(), bold()), area);
    let room = usize::from(area.width).saturating_sub(title.width() + 2);
    let note = if note.width() <= room {
        note
    } else {
        fit(&note, room).trim_end().to_owned()
    };
    frame.render_widget(Line::styled(note, style).alignment(Alignment::Right), area);
}

/// The fewest rows the repositories and latest jobs keep: a title and four jobs.
const LISTS_MIN: u16 = 5;

/// Top to bottom as the dashboard's overview: health, budget, slots, the queue, then the repositories and latest
/// jobs; the slice's charts, last on the dashboard, take what the lists leave and are the first to go.
fn overview(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let slots = app.slots().count();
    let columns = usize::from(area.width / 30).clamp(1, slots.max(1));
    let cards = length(slots.div_ceil(columns) * 7 + 1);
    let lines = length(slots + 1);
    let queued = app.live.status.queued.len();
    // What the month's job minutes would have cost, under the health line while the slots and lists keep their room.
    let savings = app
        .savings
        .as_ref()
        .map(|savings| savings.month.headline())
        .filter(|_| area.height > 5 + lines + LISTS_MIN);
    let savings_height = u16::from(savings.is_some());
    let rest = area.height.saturating_sub(4 + savings_height);
    let queue_least = if queued == 0 {
        0
    } else {
        length(queued.min(3) + 1)
    };
    let as_cards = rest >= cards + queue_least + LISTS_MIN;
    let slots_height = if as_cards { cards } else { lines };
    let left = rest.saturating_sub(slots_height);
    let queue_height = if queued == 0 {
        0
    } else {
        length(queued + 1)
            .min(left.saturating_sub(LISTS_MIN))
            .max(left.min(2))
    };
    let left = left.saturating_sub(queue_height);
    let side_by_side = area.width >= 100;
    let lists_wanted = lists_height(app, side_by_side);
    // The charts take what the lists leave, from a chart of four rows.
    let charts_height = if left >= 6 + LISTS_MIN {
        left.saturating_sub(lists_wanted).max(6)
    } else {
        0
    };
    let [
        health_area,
        savings_area,
        budget_area,
        slots_area,
        queue_area,
        lists_area,
        charts_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(savings_height),
        Constraint::Length(3),
        Constraint::Length(slots_height),
        Constraint::Length(queue_height),
        Constraint::Min(0),
        Constraint::Length(charts_height),
    ])
    .areas(area);
    health(frame, health_area, app, now, theme);
    if let Some(savings) = savings {
        frame.render_widget(
            Line::from(Span::styled(
                fit(&savings, usize::from(savings_area.width)),
                theme.dim(),
            )),
            savings_area,
        );
    }
    budget(frame, budget_area, app, theme);
    if as_cards {
        slot_cards(frame, slots_area, app, columns, now, theme);
    } else {
        slot_lines(frame, slots_area, app, now, theme);
    }
    if queue_height > 0 {
        queue(frame, queue_area, app, now, theme);
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
    repositories(frame, repositories_area, app, theme);
    latest(frame, latest_area, app, now, theme);
}

/// A count of rows or cells as a height or width.
fn length(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

/// The rows the repositories and latest jobs would fill, side by side or one above the other.
fn lists_height(app: &App, side_by_side: bool) -> u16 {
    let repositories = app.live.status.repositories.len() + 1;
    let latest = app.live.recent.len() + 1;
    length(if side_by_side {
        repositories.max(latest)
    } else {
        repositories + latest
    })
}

/// The figures that say whether CI keeps up, as the dashboard opens with them: slots running, jobs waiting and the
/// oldest's wait, then over the last 24 hours the p95 wait with the longest wait of each hour, and the failures, then
/// how many warning insights hold. What does not fit goes whole, least telling first: where to read the insights, the
/// starting and warm runners, the finished count, the hours, the oldest wait and the insights.
fn health(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
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

/// The health line's figures, each with its rank for [`drop_least`]; `None` marks where the hours' bars go.
fn health_items(app: &App, now: u64, theme: Theme) -> Vec<(u8, Option<Vec<Span<'static>>>)> {
    let health = app.live.status.health(now);
    let (running, slots) = (health.running, health.slots);
    let mut idle = String::new();
    for (count, what) in [(health.starting, "starting"), (health.warm, "warm")] {
        if count > 0 {
            let _ = write!(idle, ", {count} {what}");
        }
    }
    let sep = || Span::styled(" · ", theme.dim());
    let mut items: Vec<(u8, Option<Vec<Span<'static>>>)> = vec![(
        0,
        Some(vec![
            Span::styled(running.to_string(), bold()),
            Span::raw(format!("/{slots} running")),
        ]),
    )];
    if !idle.is_empty() {
        items.push((4, Some(vec![Span::styled(idle, theme.dim())])));
    }
    items.push((
        0,
        Some(vec![
            sep(),
            Span::styled(
                health.waiting.to_string(),
                match health.waiting_tone() {
                    api::Tone::Warn => theme.wait().add_modifier(Modifier::BOLD),
                    _ => bold(),
                },
            ),
            Span::raw(" waiting"),
        ]),
    ));
    if let Some(oldest) = health.oldest_wait {
        items.push((
            1,
            Some(vec![Span::raw(format!(", oldest {}", duration(oldest)))]),
        ));
    }
    if let Some(day) = &app.activity {
        items.push((
            0,
            Some(vec![
                sep(),
                Span::styled("24 h: ", theme.dim()),
                Span::raw("wait p95 "),
                Span::styled(
                    day.wait_p95().map_or_else(|| "–".to_owned(), duration),
                    bold(),
                ),
            ]),
        ));
        if day.wait_hours().iter().any(|wait| *wait > 0) {
            items.push((2, None));
        }
        items.push((
            0,
            Some(vec![
                sep(),
                Span::styled(
                    day.failed.to_string(),
                    match day.failed_tone() {
                        api::Tone::Neutral => bold(),
                        tone => theme.tone(tone.into()),
                    },
                ),
                Span::raw(" failed"),
            ]),
        ));
        items.push((
            3,
            Some(vec![Span::raw(format!(" of {} finished", day.finished))]),
        ));
    }
    items.extend(insight_items(app, theme));
    items
}

/// How many warning insights hold, and where to read them; nothing without one.
fn insight_items(app: &App, theme: Theme) -> Vec<(u8, Option<Vec<Span<'static>>>)> {
    let sep = || Span::styled(" · ", theme.dim());
    let mut items = Vec::new();
    let warnings = app.insights.as_ref().map_or(0, Insights::warning_count);
    if warnings > 0 {
        let what = if warnings == 1 { "insight" } else { "insights" };
        items.push((
            1,
            Some(vec![
                sep(),
                Span::styled(
                    warnings.to_string(),
                    theme.wait().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" {what}")),
            ]),
        ));
        items.push((
            5,
            Some(vec![Span::styled(": runnerctl insights", theme.dim())]),
        ));
    }
    items
}

/// `values` in `width` buckets, each the largest of the values it covers, in order.
fn resample(values: &[u64], width: usize) -> Vec<u64> {
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
fn reason_style(kind: WaitKind, theme: Theme) -> Style {
    match kind {
        WaitKind::Act => theme.wait().add_modifier(Modifier::BOLD),
        WaitKind::Soon | WaitKind::Other => theme.dim(),
        WaitKind::Full => theme.wait(),
    }
}

/// Why `job` waits in `width` cells, in the daemon's words as the dashboard shows them; nothing from a daemon that
/// does not say.
fn reason(app: &App, job: &QueuedJob, now: u64, width: usize, theme: Theme) -> Span<'static> {
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
fn queue(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
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
            format!("{} queued, oldest first; {hidden} more below", queued.len())
        } else {
            format!("{} queued, oldest first", queued.len())
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
                    cells.push(Cell::from(Span::styled(
                        fit(&place, repository_width),
                        theme.dim(),
                    )));
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
}

/// `part` of `whole` as cells of `width`.
fn scale(part: u64, whole: u64, width: u16) -> u16 {
    let cells = u128::from(part) * u128::from(width) / u128::from(whole.max(1));
    u16::try_from(cells).unwrap_or(width).min(width)
}

/// The budget's heading: what is reserved of what, how runners are admitted, and what the slice uses, each with its
/// rank for [`fit_details`].
fn budget_note(app: &App) -> Vec<(u8, String)> {
    let budget = &app.live.status.budget;
    let mut parts = vec![(0, budget.memory_reading().line())];
    if let Some(cpu) = budget.cpu {
        parts.push((1, cpu.line()));
    }
    if let Some(host) = budget.host_line() {
        parts.push((1, host));
    }
    if let Some(host) = &app.live.host {
        parts.push((2, format!("{} in use, sampled", bytes(host.memory))));
    }
    parts
}

/// The budget as one bar: a block per slot as wide as its memory (under measured admission, a block per runner as
/// wide as what it reserves), filled as far as its job uses it; then the mirror and each seed container, then what
/// is free and what is closed.
fn budget(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let status = &app.live.status;
    let mut note = budget_note(app);
    let [title, bar, labels] = Layout::vertical([Constraint::Length(1); 3]).areas(area);

    let parts = status.budget_parts();
    let blocks: Vec<_> = parts
        .slots
        .iter()
        .filter_map(|part| {
            let profile = status.profiles.get(part.profile)?;
            let slot = profile.slots.iter().find(|slot| slot.slot == part.slot)?;
            Some((part.profile, profile, slot, part.memory))
        })
        .collect();
    let whole = parts.whole;
    let mut cells = Vec::new();
    let mut names = Vec::new();
    let mut drawn = 0;
    for (index, profile, slot, memory) in blocks {
        let width = scale(memory, whole, bar.width).max(1);
        let style = theme.profile(Some(index));
        let sample =
            slot_job(app, profile, slot).and_then(|job| app.live.samples.get(&job.summary.id));
        let used_cells = sample.map_or(0, |sample| scale(sample.memory, memory, width));
        let name = format!("{} {}", profile.name, slot.slot);
        let (text, labels) = if slot.runner.is_some() {
            let figure = sample.map_or(memory, |sample| sample.memory);
            let full = bytes(figure);
            let short = ByteSize(figure).short();
            let used = if sample.is_some() { " used" } else { "" };
            (
                format!(
                    "{}{}",
                    "█".repeat(usize::from(used_cells)),
                    "▓".repeat(usize::from(width - used_cells))
                ),
                vec![
                    format!("{name} {full}{used}"),
                    format!("{name} {full}"),
                    format!("{name} {short}"),
                    name.clone(),
                ],
            )
        } else {
            (
                "░".repeat(usize::from(width)),
                vec![format!("{name} idle"), name.clone()],
            )
        };
        // A gap between blocks, taken from the block itself.
        let mut text: String = text
            .chars()
            .take(usize::from(width.saturating_sub(1)))
            .collect();
        text.push(' ');
        cells.push(Span::styled(text, style));
        let room = usize::from(width.saturating_sub(1));
        // Short of room, `used` goes, then the figure shortens, then it goes whole: a figure is never cut.
        let label = labels
            .into_iter()
            .find(|label| label.width() <= room)
            .unwrap_or_default();
        names.push(Span::styled(format!("{} ", fit(&label, room)), style));
        drawn += width;
    }
    // The mirror and each seed container, named as the slots are, or in the heading when too narrow for it.
    for Held {
        style,
        label,
        memory,
    } in held(status, &parts.held, theme)
    {
        let width = scale(memory, whole, bar.width)
            .max(2)
            .min(bar.width.saturating_sub(drawn));
        let room = usize::from(width.saturating_sub(1));
        if label.width() > room {
            note.push((3, label.clone()));
        }
        if width < 2 {
            continue;
        }
        let mut text = "▒".repeat(room);
        text.push(' ');
        cells.push(Span::styled(text, style));
        let name = if label.width() <= room {
            label
        } else {
            String::new()
        };
        names.push(Span::styled(format!("{name:room$} "), style));
        drawn += width;
    }
    let rest = bar_rest(&parts, bar.width, drawn, theme);
    cells.extend(rest.cells);
    names.extend(rest.names);
    note.extend(rest.note.map(|label| (3, label)));
    let room = usize::from(title.width).saturating_sub("Budget".width() + 2);
    heading(frame, title, "Budget", fit_details(note, room), theme);
    frame.render_widget(Line::from(cells), bar);
    frame.render_widget(Line::from(names), labels);
}

/// The bar past its blocks: what is free, then what is closed, hatched and named under it or, too narrow for that,
/// in the heading as the mirror is.
struct Rest {
    cells: Vec<Span<'static>>,
    names: Vec<Span<'static>>,
    note: Option<String>,
}

/// The bar of `width` cells past the `drawn` ones, for what of `parts` is free and closed. The closed part
/// takes what rounding leaves, so the bar never shows more free than core says is.
fn bar_rest(parts: &BudgetParts, width: u16, drawn: u16, theme: Theme) -> Rest {
    let (free, closed, whole) = (parts.free.as_ref(), parts.closed.as_ref(), parts.whole);
    let rest = width.saturating_sub(drawn);
    let free_cells = match (free, closed) {
        (Some(free), Some(_)) => scale(free.memory, whole, width).min(rest.saturating_sub(1)),
        (Some(_), None) => rest,
        (None, _) => 0,
    };
    let closed = closed.map(|closed| {
        (
            rest - free_cells,
            format!("{} {}", closed.kind.name(), closed.reading.figure),
        )
    });
    let blank = if free.is_none() && closed.is_none() {
        rest
    } else {
        0
    };
    let mut out = Rest {
        cells: vec![
            Span::styled("·".repeat(usize::from(free_cells)), theme.dim()),
            Span::raw(" ".repeat(usize::from(blank))),
        ],
        names: Vec::new(),
        note: None,
    };
    if let Some((width, label)) = closed {
        out.cells
            .push(Span::styled("╱".repeat(usize::from(width)), theme.dim()));
        out.names
            .push(Span::raw(" ".repeat(usize::from(free_cells))));
        if label.width() <= usize::from(width) {
            out.names.push(Span::styled(label, theme.dim()));
        } else {
            out.note = Some(label);
        }
    }
    out
}

/// Memory the budget holds for something other than a slot.
struct Held {
    style: Style,
    label: String,
    memory: u64,
}

/// The mirror, each seed container in its profile's colour, then each shell open in a record.
fn held(status: &Status, held: &[HeldPart], theme: Theme) -> Vec<Held> {
    held.iter()
        .map(|part| {
            let style = match &part.kind {
                HeldKind::Seed { profile } => theme.profile(
                    status
                        .profiles
                        .iter()
                        .position(|status| &status.name == profile),
                ),
                HeldKind::Mirror | HeldKind::Shell { .. } => Style::default(),
            };
            Held {
                style,
                label: format!("{} {}", part.name(), bytes(part.memory)),
                memory: part.memory,
            }
        })
        .collect()
}

/// The running job in a slot.
fn slot_job<'a>(app: &'a App, profile: &ProfileStatus, slot: &SlotStatus) -> Option<&'a Job> {
    app.live.job_in(&profile.name, slot.slot)
}

/// A sparkline of the last `width` values as one line of eighth blocks.
fn spark(values: impl DoubleEndedIterator<Item = u64>, max: u64, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut bars: Vec<char> = values
        .rev()
        .take(width)
        .map(|value| {
            let level = u128::from(value) * 7 / u128::from(max.max(1));
            BARS[usize::try_from(level).unwrap_or(7).min(7)]
        })
        .collect();
    bars.reverse();
    bars.into_iter().collect()
}

/// `text` in lines of at most `width` cells, broken between words; a word longer than a line is cut by [`fit`].
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.width() + 1 + word.width() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(fit(word, width).trim_end().to_owned()),
        }
    }
    lines
}

/// `text` cut or padded to `width` terminal cells, ending in `…` when cut.
///
/// The cut is measured on the whole prefix, not per character: a sequence such as `❤️` (a character and a
/// variation selector) takes more cells than its characters do one by one.
fn fit(text: &str, width: usize) -> String {
    let mut cut = String::new();
    if text.width() <= width {
        cut.push_str(text);
    } else if width > 0 {
        for character in text.chars() {
            cut.push(character);
            if cut.width() + 1 > width {
                cut.pop();
                break;
            }
        }
        cut.push('…');
    }
    let used = cut.width();
    cut.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    cut
}

/// What a slot's job shows: memory and CPU now, their caps and their recent samples.
struct Usage<'app> {
    samples: Vec<&'app Sample>,
    memory_cap: u64,
    cpu_cap: u64,
}

impl Usage<'_> {
    fn of<'app>(app: &'app App, profile: &ProfileStatus, job: &Job) -> Usage<'app> {
        let limits = job.limits(Some(profile));
        Usage {
            samples: app
                .series
                .get(&job.summary.id)
                .map(|series| series.iter().collect())
                .unwrap_or_default(),
            memory_cap: limits.memory.map_or(profile.memory, |limit| limit.value),
            cpu_cap: limits
                .cpu
                .map_or(u64::from(profile.cpus) * 1000, |limit| limit.value),
        }
    }

    fn last(&self) -> Option<&Sample> {
        self.samples.last().copied()
    }

    fn memory(&self, width: usize) -> String {
        spark(
            self.samples.iter().map(|sample| sample.memory),
            self.memory_cap,
            width,
        )
    }

    fn cpu(&self, width: usize) -> String {
        spark(
            self.samples.iter().map(|sample| sample.cpu),
            self.cpu_cap,
            width,
        )
    }
}

/// The slots' heading: how many run a job, as the health line counts them, and how many jobs wait.
fn slots_note(app: &App, now: u64) -> String {
    let health = app.live.status.health(now);
    format!("{} running · {} queued", health.running, health.waiting)
}

/// `line` as a sentence, its first letter a capital.
fn sentence(line: &str) -> String {
    let mut chars = line.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// `3/9 Run tests`: the step a job is in.
fn step_label(job: &Job) -> Option<(String, String)> {
    watch::current_step(job)
        .map(|(step, position)| (format!("{position}/{}", job.steps.len()), step.name.clone()))
}

fn slot_lines(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let [title, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    heading(frame, title, "Slots", slots_note(app, now), theme);
    let focused = app.pane == Pane::Slots;
    let width = usize::from(rest.width);
    let lines: Vec<Line<'_>> = app
        .slots()
        .enumerate()
        .map(|(position, (index, profile, slot))| {
            let style = theme.profile(Some(index));
            let mut spans = vec![
                Span::styled("■ ", style),
                Span::styled(fit(&format!("{} {}", profile.name, slot.slot), 9), bold()),
            ];
            let state = Status::slot_state(profile, slot);
            match slot_job(app, profile, slot).filter(|_| state.is_running()) {
                Some(job) => {
                    let usage = Usage::of(app, profile, job);
                    let (count, name) = step_label(job).unwrap_or_default();
                    let ran = job
                        .summary
                        .timeline
                        .started_at
                        .map_or_else(String::new, |at| duration(now.saturating_sub(at)));
                    let memory = usage.last().map_or_else(String::new, |s| bytes(s.memory));
                    let cpu = usage.last().map_or_else(String::new, |s| cores_used(s.cpu));
                    // Fixed columns at the right, their sparklines only from 100 columns; the job's name takes what
                    // is left, then its repository if that fits too.
                    let sparks = width >= 100;
                    let job_width = width
                        .saturating_sub(11 + 7 + 16 + 9 + 7 + 6 + if sparks { 13 } else { 0 })
                        .max(10);
                    let title = job.summary.title().to_string();
                    let repository = &job.summary.repository;
                    let (title, repository) = if title.width() + 1 + repository.width() <= job_width
                    {
                        let pad = job_width - title.width() - 1;
                        (format!("{title} "), fit(repository, pad))
                    } else {
                        (fit(&title, job_width), String::new())
                    };
                    spans.extend([
                        Span::raw(title),
                        Span::styled(repository, theme.dim()),
                        Span::styled(format!(" {count:>5} "), theme.cpu()),
                        Span::raw(fit(&name, 16)),
                        Span::raw(format!(" {ran:>7} ")),
                        Span::raw(format!("{memory:>6} ")),
                    ]);
                    if sparks {
                        spans.push(Span::styled(usage.memory(6), theme.memory()));
                    }
                    spans.push(Span::raw(format!("{cpu:>5} ")));
                    if sparks {
                        spans.push(Span::styled(usage.cpu(6), theme.cpu()));
                    }
                }
                None => spans.push(Span::styled(state.line(), theme.dim())),
            }
            let line = Line::from(spans);
            if focused && position == app.slot {
                line.style(selected())
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rest);
}

fn slot_cards(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    columns: usize,
    now: u64,
    theme: Theme,
) {
    let [title, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    heading(frame, title, "Slots", slots_note(app, now), theme);
    let slots: Vec<_> = app.slots().collect();
    let rows =
        Layout::vertical(vec![Constraint::Length(7); slots.len().div_ceil(columns)]).split(rest);
    let cells = |row| {
        Layout::horizontal(vec![
            Constraint::Ratio(
                1,
                u32::try_from(columns).unwrap_or(1)
            );
            columns
        ])
        .split(row)
    };
    // Every title keeps at least a cell of its rule: the size drops its CPUs, then itself, alike on every card,
    // before a name is cut. Left for the name: the width less the corners, the rule's cell, ` ■ ` and a space.
    let name_width = usize::from(cells(rest).iter().map(|cell| cell.width).min().unwrap_or(0))
        .saturating_sub(2 + 1 + 3 + 1);
    let longest = slots
        .iter()
        .map(|(_, profile, slot)| card_title(profile, slot).width())
        .max()
        .unwrap_or(0);
    let detail = (0..2)
        .find(|&detail| {
            slots
                .iter()
                .all(|(_, profile, _)| longest + card_sizes(profile)[detail].width() <= name_width)
        })
        .unwrap_or(2);
    for (position, (index, profile, slot)) in slots.into_iter().enumerate() {
        let cell = cells(rows[position / columns])[position % columns];
        let style = theme.profile(Some(index));
        let focused = app.pane == Pane::Slots && position == app.slot;
        let block = Block::bordered()
            .border_type(if focused {
                BorderType::Thick
            } else {
                BorderType::Plain
            })
            .border_style(style)
            .title(Line::from(vec![
                Span::styled(" ■ ", style),
                Span::styled(
                    format!(
                        "{} ",
                        fit(&card_title(profile, slot), name_width).trim_end()
                    ),
                    if focused { selected() } else { bold() },
                ),
            ]))
            .title_top(
                Line::styled(card_sizes(profile)[detail].clone(), theme.dim()).right_aligned(),
            );
        let inner = block.inner(cell);
        frame.render_widget(block, cell);
        let lines = card(app, profile, slot, usize::from(inner.width), now, theme);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// A slot card's name in its title, such as `jvm · slot 1`.
fn card_title(profile: &ProfileStatus, slot: &SlotStatus) -> String {
    format!("{} · slot {}", profile.name, slot.slot)
}

/// A slot card's size in its title, from the fullest to none.
fn card_sizes(profile: &ProfileStatus) -> [String; 3] {
    [
        format!(" {} · {} CPUs ", bytes(profile.memory), profile.cpus),
        format!(" {} ", bytes(profile.memory)),
        String::new(),
    ]
}

/// What a slot card shows: its job with step, time, memory and CPU; its runner waiting for its job, or warm for the
/// next one; or nothing.
fn card(
    app: &App,
    profile: &ProfileStatus,
    slot: &SlotStatus,
    width: usize,
    now: u64,
    theme: Theme,
) -> Vec<Line<'static>> {
    let state = Status::slot_state(profile, slot);
    match (slot_job(app, profile, slot), &slot.runner) {
        (Some(job), _) if state.is_running() => {
            let usage = Usage::of(app, profile, job);
            let (count, name) = step_label(job).unwrap_or_default();
            let ran = job
                .summary
                .timeline
                .started_at
                .map_or_else(String::new, |at| duration(now.saturating_sub(at)));
            let memory = usage
                .last()
                .map_or_else(|| "–".to_owned(), |s| bytes(s.memory));
            let cpu = usage
                .last()
                .map_or_else(|| "–".to_owned(), |s| cores_used(s.cpu));
            let half = width.saturating_sub(1) / 2;
            vec![
                Line::styled(fit(&job.summary.repository, width), theme.dim()),
                Line::styled(fit(&job.summary.title().to_string(), width), bold()),
                Line::from(vec![
                    Span::styled(
                        format!("{count} "),
                        theme.cpu().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(fit(&name, width.saturating_sub(count.width() + 1))),
                ]),
                Line::from(vec![
                    Span::raw(format!("{ran:<8}")),
                    Span::styled("mem ", theme.dim()),
                    Span::raw(format!("{memory:<7}")),
                    Span::styled("cpu ", theme.dim()),
                    Span::raw(cpu),
                ]),
                Line::from(vec![
                    Span::styled(format!("{:<half$}", usage.memory(half)), theme.memory()),
                    Span::raw(" "),
                    Span::styled(usage.cpu(half), theme.cpu()),
                ]),
            ]
        }
        (_, Some(runner)) => vec![
            Line::styled(fit(&sentence(&state.line()), width), theme.dim()),
            Line::styled(
                format!("started {}", human::ago(runner.started_at, now)),
                theme.dim(),
            ),
        ],
        (_, None) => vec![Line::styled(sentence(&state.line()), theme.dim())],
    }
}

/// The slice's memory and CPU, every runner container together, as filled charts against their limits.
fn slice(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let [memory_area, cpu_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let status = &app.live.status;
    let memory_max = status.budget.memory_max;
    Chart {
        title: "Memory",
        limit_label: memory_max.map_or_else(String::new, |max| format!("MemoryMax {}", bytes(max))),
        reserved_label: String::new(),
        values: app.host.iter().map(|sample| sample.memory).collect(),
        limit: memory_max.unwrap_or(0),
        unit: bytes,
        style: theme.memory(),
    }
    .render(frame, memory_area, theme);
    Chart {
        title: "CPU",
        // Up to the CPUs the slice may use, as the dashboard draws it: the slots' quotas may add up past them.
        limit_label: status
            .budget
            .slice_cpus
            .map_or_else(String::new, |slice| format!("slice {}", cpus(slice))),
        reserved_label: String::new(),
        values: app.host.iter().map(|sample| sample.cpu).collect(),
        limit: status.budget.slice_cpus.unwrap_or(0),
        unit: in_cores,
        style: theme.cpu(),
    }
    .render(frame, cpu_area, theme);
}

/// A filled chart of the newest values that fit, scaled to `limit` (or to the values' peak when there is none),
/// with the last value in its title and the peak under it.
struct Chart {
    title: &'static str,
    limit_label: String,
    /// Under it on the right, such as a job's reservation.
    reserved_label: String,
    values: Vec<u64>,
    limit: u64,
    unit: fn(u64) -> String,
    style: Style,
}

impl Chart {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: Theme) {
        let unit = self.unit;
        let peak = self.values.iter().copied().max();
        let block = Block::bordered()
            .border_style(theme.dim())
            .title(Line::from(vec![
                Span::styled(format!(" {} ", self.title), bold()),
                Span::raw(
                    self.values
                        .last()
                        .map_or_else(String::new, |last| format!("{} ", unit(*last))),
                ),
            ]))
            .title_top(
                Line::styled(
                    if self.limit_label.is_empty() {
                        String::new()
                    } else {
                        format!(" {} ", self.limit_label)
                    },
                    theme.dim(),
                )
                .right_aligned(),
            )
            .title_bottom(Line::styled(
                peak.map_or_else(
                    || " no samples yet ".to_owned(),
                    |peak| format!(" sampled peak {} ", unit(peak)),
                ),
                theme.dim(),
            ))
            .title_bottom(
                Line::styled(
                    if self.reserved_label.is_empty() {
                        String::new()
                    } else {
                        format!(" {} ", self.reserved_label)
                    },
                    theme.dim(),
                )
                .right_aligned(),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let shown = &self.values[self.values.len().saturating_sub(usize::from(inner.width))..];
        let max = if self.limit > 0 {
            self.limit
        } else {
            peak.unwrap_or(1)
        };
        frame.render_widget(
            Sparkline::default().data(shown).max(max).style(self.style),
            inner,
        );
    }
}

fn list_block(title: &str, focused: bool, theme: Theme) -> Block<'_> {
    Block::new()
        .borders(Borders::TOP)
        .border_style(if focused { bold() } else { theme.dim() })
        .title(Span::styled(
            format!(" {title} "),
            if focused { selected() } else { bold() },
        ))
}

/// What the repositories pane needs to show each repository whole with its profile, and a cell before the next pane.
fn repositories_width(app: &App) -> u16 {
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

fn repositories(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let focused = app.pane == Pane::Repositories;
    let repositories = &app.live.status.repositories;
    // A cell stays free before the latest jobs: the profiles, which the square's colour also shows for the first
    // pool, go from every line first, then a name is cut.
    let width = usize::from(area.width).saturating_sub(1);
    let spans = |repository: &RepositoryStatus, profile: Option<&str>, name_width: usize| {
        let mut spans = vec![
            Span::styled("■ ", theme.profile(app.profile_index(&repository.profile))),
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
    frame.render_widget(
        Paragraph::new(lines).block(list_block("Repositories", focused, theme)),
        area,
    );
}

/// An outcome as an upper-case badge.
fn badge(job: &JobSummary, theme: Theme) -> Span<'static> {
    let (word, tone) = human::outcome(job);
    Span::styled(
        word.to_uppercase(),
        theme.tone(tone).add_modifier(Modifier::BOLD),
    )
}

/// The cells the widest of `jobs`' badges takes, so a column of them never cuts an outcome such as `startup
/// failure`.
fn badge_width<'job>(jobs: impl IntoIterator<Item = &'job JobSummary>) -> usize {
    jobs.into_iter()
        .map(|job| job.outcome().label.width())
        .max()
        .unwrap_or(0)
}

/// How long ago a job finished, or nothing while it has not.
fn finished_ago(job: &JobSummary, now: u64) -> String {
    job.timeline
        .finished_at
        .map_or_else(String::new, |at| human::ago(at, now))
}

/// The latest finished jobs, newest first: the outcome, the job, its repository where there is room, from 110 columns
/// how long it waited and from 140 its branch, how long it ran and when it finished.
fn latest(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let focused = app.pane == Pane::Latest;
    let block = list_block("Latest jobs", focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let jobs = &app.live.recent;
    let run_times: Vec<String> = jobs
        .iter()
        .map(|job| job.ran(now).map_or_else(String::new, duration))
        .collect();
    let finished: Vec<String> = jobs.iter().map(|job| finished_ago(job, now)).collect();
    // The times are as wide as the widest of them; the job and its repository take what is left, the job first.
    let ran_width = run_times.iter().map(|ran| ran.width()).max().unwrap_or(0);
    let ago_width = finished.iter().map(|ago| ago.width()).max().unwrap_or(0);
    let waits: Vec<String> = jobs
        .iter()
        .map(|job| job.waited(now).map_or_else(String::new, duration))
        .collect();
    let wait_width = if inner.width >= 110 {
        waits.iter().map(|wait| wait.width()).max().unwrap_or(0)
    } else {
        0
    };
    let branch_width = if inner.width >= 140 {
        jobs.iter()
            .map(|job| job.branch.as_deref().unwrap_or_default().width())
            .max()
            .unwrap_or(0)
            .min(20)
    } else {
        0
    };
    let optional = [wait_width, branch_width]
        .iter()
        .filter(|width| **width > 0)
        .map(|width| width + 1)
        .sum::<usize>();
    let outcome_width = badge_width(jobs);
    let rest = usize::from(inner.width)
        .saturating_sub(outcome_width + 1 + ran_width + 1 + ago_width + 1 + optional);
    let titles: Vec<String> = jobs.iter().map(|job| job.title().to_string()).collect();
    let title_width = titles.iter().map(|title| title.width()).max().unwrap_or(0);
    let repository_width = jobs
        .iter()
        .map(|job| job.repository.width())
        .max()
        .unwrap_or(0);
    // The repository shows from a dozen cells, cut if it must be.
    let (title_width, repository_width) = if rest >= title_width + 1 + repository_width.min(12) {
        (title_width, rest - title_width - 1)
    } else {
        (rest, 0)
    };
    let rows = jobs
        .iter()
        .zip(titles)
        .zip(run_times.into_iter().zip(finished))
        .zip(waits)
        .map(|(((job, title), (ran, ago)), wait)| {
            let mut cells = vec![
                Cell::from(badge(job, theme)),
                Cell::from(fit(&title, title_width)),
            ];
            if repository_width > 0 {
                cells.push(Cell::from(Span::styled(
                    fit(&job.repository, repository_width),
                    theme.dim(),
                )));
            }
            if branch_width > 0 {
                cells.push(Cell::from(Span::styled(
                    fit(job.branch.as_deref().unwrap_or_default(), branch_width),
                    theme.dim(),
                )));
            }
            if wait_width > 0 {
                cells.push(Cell::from(
                    Line::from(Span::styled(wait, theme.wait())).right_aligned(),
                ));
            }
            cells.extend([
                Cell::from(Line::from(ran).right_aligned()),
                Cell::from(Line::from(Span::styled(ago, theme.dim())).right_aligned()),
            ]);
            Row::new(cells)
        });
    let mut widths = vec![
        Constraint::Length(length(outcome_width)),
        Constraint::Length(length(title_width)),
    ];
    for width in [repository_width, branch_width, wait_width] {
        if width > 0 {
            widths.push(Constraint::Length(length(width)));
        }
    }
    widths.extend([
        Constraint::Length(length(ran_width)),
        Constraint::Length(length(ago_width)),
    ]);
    let table = Table::new(rows, widths).row_highlight_style(selected());
    let mut state = TableState::default().with_selected(focused.then_some(app.latest));
    frame.render_stateful_widget(table, inner, &mut state);
}

fn history(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let filters = &app.history;
    let [title, table_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    heading(
        frame,
        title,
        "History",
        history_note(
            filters,
            usize::from(title.width).saturating_sub("History".width() + 2),
        ),
        theme,
    );
    let Some(jobs) = &filters.page else {
        frame.render_widget(Line::styled("reading…", theme.dim()), table_area);
        return;
    };
    if jobs.jobs.is_empty() {
        frame.render_widget(Line::styled("no jobs", theme.dim()), table_area);
        return;
    }
    let wide = area.width >= 110;
    let finished = jobs
        .jobs
        .iter()
        .map(|job| finished_ago(job, now).width())
        .chain(["FINISHED".len()])
        .max()
        .unwrap_or(0);
    let outcome_width = badge_width(&jobs.jobs).max("OUTCOME".len());
    let mut columns = vec![("OUTCOME", outcome_width), ("REPOSITORY", 0), ("JOB", 0)];
    if wide {
        columns.push(("BRANCH", 14));
    }
    columns.extend([("WAITED", 7), ("RAN", 8), ("MEM", 6)]);
    if wide {
        columns.push(("CPU", 5));
    }
    columns.push(("FINISHED", finished));
    // The repository and the job share what the other columns and the spaces between them leave.
    let fixed: usize = columns.iter().map(|(_, width)| width + 1).sum::<usize>() - 1;
    let shared = usize::from(table_area.width).saturating_sub(fixed);
    let repository_width = shared / 2;
    let job_width = shared - repository_width;
    let rows = jobs.jobs.iter().map(|job| {
        let mut cells = vec![
            Cell::from(badge(job, theme)),
            Cell::from(fit(&job.repository, repository_width)),
            Cell::from(fit(&job.title().to_string(), job_width)),
        ];
        if wide {
            cells.push(Cell::from(fit(
                job.branch.as_deref().unwrap_or_default(),
                14,
            )));
        }
        cells.extend([
            Cell::from(job.waited(now).map_or_else(String::new, duration)),
            Cell::from(job.ran(now).map_or_else(String::new, duration)),
            Cell::from(job.metrics.memory_peak.map_or_else(String::new, bytes)),
        ]);
        if wide {
            cells.push(Cell::from(
                job.metrics.cpu_average.map_or_else(String::new, cores_used),
            ));
        }
        cells.push(Cell::from(Span::styled(
            finished_ago(job, now),
            theme.dim(),
        )));
        Row::new(cells)
    });
    let header: Vec<_> = columns.iter().map(|(name, _)| *name).collect();
    let widths: Vec<_> = columns
        .iter()
        .map(|(name, width)| match *name {
            "REPOSITORY" => Constraint::Length(u16::try_from(repository_width).unwrap_or(0)),
            "JOB" => Constraint::Fill(1),
            _ => Constraint::Length(u16::try_from(*width).unwrap_or(u16::MAX)),
        })
        .collect();
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(theme.dim()))
        .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(filters.selected));
    frame.render_stateful_widget(table, table_area, &mut state);
}

/// The history's filters and page, as fit `width`.
fn history_note(filters: &History, width: usize) -> String {
    let any = |filter: &Option<String>| filter.clone().unwrap_or_else(|| "any".to_owned());
    fit_details(
        vec![
            (0, format!("outcome {}", any(&filters.conclusion))),
            (0, format!("repository {}", any(&filters.repository))),
            (1, format!("workflow {}", any(&filters.workflow))),
            (2, format!("page {}", filters.previous.len() + 1)),
        ],
        width,
    )
}

/// The records of failed jobs, each a stopped runner `runnerctl shell` opens a shell in.
fn records(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let [title, table_area, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let Some(records) = &app.records else {
        heading(frame, title, "Records", String::new(), theme);
        frame.render_widget(Line::styled("reading…", theme.dim()), table_area);
        return;
    };
    // Near max_total, that comes first and stands out: the usage is what a narrow heading drops.
    match records.pressure() {
        Some(pressure) => {
            let room = usize::from(title.width).saturating_sub("Records".width() + 2);
            let note = fit_details(vec![(0, pressure.to_owned()), (1, records.usage())], room);
            styled_heading(frame, title, "Records", note, theme.wait());
        }
        None => heading(frame, title, "Records", records.usage(), theme),
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
    .header(Row::new(["JOB", "REPOSITORY", "JOB NAME", "SIZE", "MADE"]).style(theme.dim()))
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

/// What each profile's jobs use and what to set its memory and slots to; the selected profile's jobs below. Under
/// 100 columns the medians, the jobs at their cap, the largest peaks and the waits of warm and cold starts are left
/// out.
fn sizing(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let Some(sizing) = &app.sizing else {
        let [title, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        heading(frame, title, "Sizing", String::new(), theme);
        frame.render_widget(Line::styled("reading…", theme.dim()), rest);
        return;
    };
    let wide = area.width >= 100;
    let profiles = length(sizing.profiles.len());
    let plan = auto_plan(sizing, usize::from(area.width), now, app, theme);
    let [title, profiles_area, plan_area, advice_area, jobs_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(profiles.saturating_add(1)),
        Constraint::Length(length(plan.len())),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(plan), plan_area);
    heading(
        frame,
        title,
        "Sizing",
        fit_details(
            [(0, format!("{} of history", duration(sizing.covered())))]
                .into_iter()
                .chain([1, 2, 2].into_iter().zip(sizing.method_parts()))
                .chain([(
                    if sizing.auto_plan.is_some() { 3 } else { 0 },
                    format!("auto sizing {}", if sizing.auto { "on" } else { "off" }),
                )])
                .collect(),
            usize::from(title.width).saturating_sub("Sizing".width() + 2),
        ),
        theme,
    );
    sizing_profiles(frame, profiles_area, app, sizing, theme);
    let Some(profile) = sizing.profiles.get(app.sized) else {
        return;
    };
    let width = usize::from(advice_area.width);
    // The advice wraps onto a second line rather than lose its end; the hint below it is cut.
    let mut lines = vec![Line::styled(
        format!("{}: {}", profile.name, sizing.advice_line(profile)),
        theme.tone(profile.advice.tone().into()),
    )];
    let last = sizing
        .changes
        .iter()
        .rfind(|change| change.profile == profile.name);
    if let Some(change) = profile.advice.change().filter(|_| !sizing.auto) {
        lines.push(Line::styled(
            fit(
                &format!(
                    "a writes memory = \"{}\", slots = {} into the config; the daemon checks it first",
                    ByteSize(change.memory),
                    change.slots
                ),
                width,
            ),
            theme.dim(),
        ));
    } else if let Some(change) = last {
        lines.push(Line::styled(
            fit(
                &format!(
                    "auto sizing {}: {}; runnerctl sizing undo {} writes it back",
                    human::ago(change.at, now),
                    change.line(),
                    profile.name
                ),
                width,
            ),
            theme.dim(),
        ));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), advice_area);
    sizing_jobs(frame, jobs_area, profile, wide, theme);
}

/// Under auto sizing, its rules, why no look changes anything if one would not, and per profile what its next look
/// changes, how long a recent change holds it, or that its advice asks for nothing; a blank line after. Nothing while
/// auto sizing is off, or from a daemon that does not say what it does next.
fn auto_plan(
    sizing: &Sizing,
    width: usize,
    now: u64,
    app: &App,
    theme: Theme,
) -> Vec<Line<'static>> {
    let Some(plan) = sizing.auto_plan.as_ref().filter(|_| sizing.auto) else {
        return Vec::new();
    };
    let mut lines = vec![Line::from("")];
    // The rules and what blocks every look are sentences, wrapped rather than cut.
    for (position, line) in wrap(&format!("Auto sizing {}", plan.rules()), width)
        .into_iter()
        .enumerate()
    {
        lines.push(
            match line.strip_prefix("Auto sizing ").filter(|_| position == 0) {
                Some(rest) => Line::from(vec![
                    Span::styled("Auto sizing ", bold()),
                    Span::styled(rest.to_owned(), theme.dim()),
                ]),
                None => Line::styled(line, theme.dim()),
            },
        );
    }
    if let Some(blocked) = &plan.blocked {
        lines.extend(
            wrap(&format!("No look changes anything now: {blocked}"), width)
                .into_iter()
                .map(|line| Line::styled(line, theme.tone(Tone::Bad))),
        );
    }
    let name_width = sizing
        .profiles
        .iter()
        .map(|profile| profile.name.width())
        .max()
        .unwrap_or(0);
    for profile in &sizing.profiles {
        let (what, why) = plan_line(sizing, plan, profile, now);
        let room = width.saturating_sub(2 + name_width + 1);
        let what_width = what.width().min(room);
        let mut spans = vec![
            Span::styled("■ ", theme.profile(app.profile_index(&profile.name))),
            Span::raw(format!("{:<name_width$} ", profile.name)),
            Span::raw(fit(&what, what_width)),
        ];
        let rest = room.saturating_sub(what_width + 2);
        if rest >= 12 {
            spans.push(Span::styled(
                format!("  {}", fit(&why, rest).trim_end()),
                theme.dim(),
            ));
        }
        lines.push(Line::from(spans));
        // Beside the change, which can still be to its memory or slots.
        if let Some(refusal) = plan.refusal(&profile.name) {
            let indent = 2 + name_width + 1;
            lines.push(Line::styled(
                format!(
                    "{:indent$}{}",
                    "",
                    fit(&refusal, width.saturating_sub(indent))
                ),
                theme.tone(Tone::Busy),
            ));
        }
    }
    lines
}

/// What auto sizing does next with a profile, then why, as the dashboard's sizing page says it.
fn plan_line(
    sizing: &Sizing,
    plan: &AutoPlan,
    profile: &ProfileSizing,
    now: u64,
) -> (String, String) {
    let next = plan.profile_plan(&profile.name, now);
    let mut what = next.headline(plan);
    if let Some(until) = next.until() {
        let _ = write!(what, " for {}", duration(until.saturating_sub(now)));
    }
    let reason = next.reason(sizing, profile);
    let why = match next.changed() {
        Some(changed) => format!("changed {}; {reason}", human::ago(changed, now)),
        None => reason,
    };
    (what, why)
}

/// A row per profile: its memory and slots, its jobs' peaks, how busy and how long waited, the advice.
fn sizing_profiles(
    frame: &mut Frame<'_>,
    profiles_area: Rect,
    app: &App,
    sizing: &Sizing,
    theme: Theme,
) {
    let wide = profiles_area.width >= 100;
    let mut columns = vec![("PROFILE", 0), ("MEMORY", 10), ("JOBS", 5)];
    // Narrow, the largest peak advice is sized for stays and the percentiles go.
    if wide {
        columns.push(("P50", 6));
    }
    columns.push(("P95", 6));
    if wide {
        columns.push(("P99", 6));
    }
    columns.push(("MAX", 6));
    if wide {
        columns.push(("AT CAP", 6));
    }
    columns.extend([("CPU P99", 7), ("BUSY", 5), ("WAIT P95", 8)]);
    if wide {
        columns.push(("P50 WARM/COLD", 13));
    }
    columns.push(("ADVICE", 11));
    let rows = sizing.profiles.iter().map(|profile| {
        let peaks = &profile.peaks;
        let mut cells = vec![
            Cell::from(Span::styled(
                profile.name.clone(),
                theme.profile(app.profile_index(&profile.name)),
            )),
            Cell::from(format!("{} × {}", bytes(profile.memory), profile.slots)),
            Cell::from(peaks.jobs.to_string()),
        ];
        if wide {
            cells.push(Cell::from(bytes(peaks.memory.p50)));
        }
        cells.push(Cell::from(bytes(peaks.memory.p95)));
        if wide {
            cells.push(Cell::from(bytes(peaks.memory.p99)));
        }
        cells.push(Cell::from(bytes(peaks.memory.max)));
        if wide {
            cells.push(Cell::from(if peaks.at_cap > 0 {
                Span::styled(peaks.at_cap.to_string(), theme.tone(Tone::Bad))
            } else {
                Span::raw("")
            }));
        }
        cells.extend([
            Cell::from(cores_used(peaks.cpu.p99)),
            Cell::from(format!("{}%", profile.busy)),
            Cell::from(duration(profile.wait.p95)),
        ]);
        if wide {
            let starts = &profile.starts;
            cells.push(Cell::from(if starts.warm > 0 {
                format!(
                    "{} / {}",
                    duration(starts.warm_p50),
                    duration(starts.cold_p50)
                )
            } else {
                String::new()
            }));
        }
        cells.push(Cell::from(Span::styled(
            profile.advice.word(),
            theme.tone(profile.advice.tone().into()),
        )));
        Row::new(cells)
    });
    let table = Table::new(rows, widths(&columns))
        .header(Row::new(columns.iter().map(|(name, _)| *name)).style(theme.dim()))
        .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(app.sized));
    frame.render_stateful_widget(table, profiles_area, &mut state);
}

/// The selected profile's jobs, a row each.
fn sizing_jobs(
    frame: &mut Frame<'_>,
    jobs_area: Rect,
    profile: &ProfileSizing,
    wide: bool,
    theme: Theme,
) {
    let mut columns = vec![("REPOSITORY", 0), ("JOB", 0), ("RUNS", 5)];
    columns.push(("P95", 6));
    if wide {
        columns.push(("P99", 6));
    }
    columns.extend([("MAX", 6), ("CPU P99", 7), ("WAIT P95", 8), ("NEEDS", 11)]);
    // The repository and the job share what the other columns and the spaces between them leave.
    let fixed: usize = columns.iter().map(|(_, width)| width + 1).sum::<usize>() - 1;
    let shared = usize::from(jobs_area.width).saturating_sub(fixed);
    let repository_width = shared / 2;
    let rows = profile.jobs.iter().map(|job| {
        let needs = job.needs();
        let needs = Span::styled(needs.text, theme.tone(needs.tone.into()));
        let name = job_title(job.workflow.as_deref(), &job.name, None).to_string();
        let mut cells = vec![
            Cell::from(fit(&job.repository, repository_width)),
            Cell::from(fit(&name, shared - repository_width)),
            Cell::from(job.peaks.jobs.to_string()),
            Cell::from(bytes(job.peaks.memory.p95)),
        ];
        if wide {
            cells.push(Cell::from(bytes(job.peaks.memory.p99)));
        }
        cells.extend([
            Cell::from(bytes(job.peaks.memory.max)),
            Cell::from(cores_used(job.peaks.cpu.p99)),
            Cell::from(duration(job.wait.p95)),
            Cell::from(needs),
        ]);
        Row::new(cells)
    });
    let mut constraints = widths(&columns);
    constraints[0] = Constraint::Length(u16::try_from(repository_width).unwrap_or(0));
    let table = Table::new(rows, constraints)
        .header(Row::new(columns.iter().map(|(name, _)| *name)).style(theme.dim()));
    frame.render_widget(table, jobs_area);
}

/// A table's column widths: 0 fills what the others leave.
fn widths(columns: &[(&str, usize)]) -> Vec<Constraint> {
    columns
        .iter()
        .map(|(_, width)| match width {
            0 => Constraint::Fill(1),
            width => Constraint::Length(u16::try_from(*width).unwrap_or(u16::MAX)),
        })
        .collect()
}

fn job(frame: &mut Frame<'_>, area: Rect, app: &App, open: &JobView, now: u64, theme: Theme) {
    let job = &open.job;
    let summary = &job.summary;
    // A queued job has no steps yet, nor their heading.
    let steps_wanted = if job.steps.is_empty() {
        0
    } else {
        length(job.steps.len() + 1)
    };
    let runs_height = if open.runs.as_ref().is_some_and(|runs| runs.len() > 1) {
        2
    } else {
        0
    };
    let fixed = 3 + 2 + runs_height;
    // Until a runner has it there is nothing to chart.
    let charted = summary.runner.is_some() || !open.samples.is_empty();
    let full_charts = charted && area.height >= fixed + steps_wanted + 7 + 6;
    let charts_height = match (charted, full_charts) {
        (false, _) => 0,
        (true, true) => 7,
        (true, false) => 2,
    };
    let steps_height = steps_wanted.min(area.height.saturating_sub(fixed + charts_height + 4));
    let [
        head,
        timeline_area,
        runs_area,
        steps_area,
        charts_area,
        log_area,
    ] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Length(runs_height),
        Constraint::Length(steps_height),
        Constraint::Length(charts_height),
        Constraint::Min(0),
    ])
    .areas(area);

    let width = usize::from(head.width);
    let details = fit_details(job_details(app, job, now), width);
    let badge = badge(summary, theme);
    let title = summary.title().to_string();
    let title_width = title.width().min(width.saturating_sub(badge.width() + 2));
    let style = summary.runner.as_ref().map_or_else(
        || theme.dim(),
        |runner| theme.profile(app.profile_index(&runner.profile)),
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("■ ", style),
                Span::styled(
                    fit(&summary.repository, width.saturating_sub(2)),
                    theme.dim(),
                ),
            ]),
            Line::from(vec![
                Span::styled(fit(&title, title_width), bold()),
                Span::raw("  "),
                badge,
            ]),
            Line::styled(details, theme.dim()),
        ]),
        head,
    );
    let waiting = app
        .live
        .status
        .queued
        .iter()
        .find(|queued| queued.id == summary.id)
        .filter(|_| summary.timeline.started_at.is_none());
    let reason = waiting.map(|queued| reason(app, queued, now, usize::from(area.width), theme));
    timeline(frame, timeline_area, summary, reason.as_ref(), now, theme);
    if runs_height > 0 {
        job_runs(
            frame,
            runs_area,
            app,
            summary,
            &job.compare(open.runs.as_deref().unwrap_or_default(), now),
            now,
            theme,
        );
    }
    steps(frame, steps_area, job, now, theme);
    match (charted, full_charts) {
        (false, _) => {}
        (true, true) => job_charts(frame, charts_area, app, open, theme),
        (true, false) => job_spark(frame, charts_area, app, open, theme),
    }
    log(frame, log_area, open, theme);
}

/// The open job's details under its name, each with its rank: when they do not fit one line, the least important
/// go first.
fn job_details(app: &App, job: &Job, now: u64) -> Vec<(u8, String)> {
    let summary = &job.summary;
    let profile = summary.runner.as_ref().and_then(|runner| {
        app.live
            .status
            .profiles
            .iter()
            .find(|p| p.name == runner.profile)
    });
    let mut details = vec![];
    if let Some(branch) = &summary.branch {
        details.push((3, format!("branch {branch}")));
    }
    details.push((5, format!("run {}", summary.run_id)));
    if let Some(runner) = &summary.runner {
        details.push((1, format!("slot {} · {}", runner.profile, runner.slot)));
        if let Some(limits) = job.limits(profile).line() {
            details.push((4, limits));
        }
    }
    if let Some(waited) = summary.waited(now) {
        details.push((2, format!("waited {}", duration(waited))));
    }
    if let Some(ran) = summary.ran(now) {
        details.push((0, format!("ran {}", duration(ran))));
    }
    if job
        .record
        .as_ref()
        .is_some_and(|note| matches!(note.state(), RecordState::Kept { .. }))
    {
        details.push((
            1,
            format!("record kept: {}", RecordNote::shell_command(summary.id)),
        ));
    }
    details
}

/// `details` joined by ` · ` in their order, without the least important (highest rank) that do not fit `width`.
fn fit_details(mut details: Vec<(u8, String)>, width: usize) -> String {
    let line = |details: &[(u8, String)]| {
        details
            .iter()
            .map(|(_, detail)| detail.as_str())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    drop_least(&mut details, 0, |details| line(details).width() <= width);
    line(&details)
}

/// Removes the least important of `items` (the highest rank, the last of equals) until they `fit`, keeping those
/// ranked under `kept`.
fn drop_least<T>(items: &mut Vec<(u8, T)>, kept: u8, fit: impl Fn(&[(u8, T)]) -> bool) {
    while !fit(items) {
        let Some(least) = items
            .iter()
            .enumerate()
            .filter(|(_, (rank, _))| *rank >= kept)
            .max_by_key(|(_, (rank, _))| *rank)
            .map(|(position, _)| position)
        else {
            return;
        };
        items.remove(least);
    }
}

/// Waiting for a runner, the runner starting, and running, as one bar; while it waits, why. Cleaning up is left
/// out: the bar is the job's way to its finish.
fn timeline(
    frame: &mut Frame<'_>,
    area: Rect,
    job: &JobSummary,
    reason: Option<&Span<'_>>,
    now: u64,
    theme: Theme,
) {
    let phases: Vec<Phase> = job
        .phases()
        .into_iter()
        .filter(|phase| phase.kind != PhaseKind::CleaningUp)
        .collect();
    let whole: u64 = phases.iter().map(|phase| phase.seconds(now)).sum();
    let look = |kind: PhaseKind| match kind {
        PhaseKind::Waiting => ("░", theme.wait()),
        PhaseKind::Starting => ("▒", theme.cpu()),
        PhaseKind::Running | PhaseKind::CleaningUp => ("█", theme.memory()),
    };
    let width = area.width;
    let mut used = 0;
    let mut bar = Vec::new();
    for (index, phase) in phases.iter().enumerate() {
        let seconds = phase.seconds(now);
        // The last phase takes what is left, so the bar fills the line.
        let cells = if index + 1 == phases.len() {
            width.saturating_sub(used)
        } else {
            scale(seconds, whole, width)
                .max(u16::from(seconds > 0))
                .min(width.saturating_sub(used))
        };
        used += cells;
        let (glyph, style) = look(phase.kind);
        bar.push(Span::styled(glyph.repeat(usize::from(cells)), style));
    }
    let [bar_area, legend] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    frame.render_widget(Line::from(bar), bar_area);
    let mut spans = vec![];
    for phase in &phases {
        let (glyph, style) = look(phase.kind);
        let still = if phase.to.is_none() { "…" } else { "" };
        spans.extend([
            Span::styled(format!("{glyph} "), style),
            Span::raw(format!(
                "{} {}{still}  ",
                phase.kind.label(),
                duration(phase.seconds(now))
            )),
        ]);
        if phase.kind == PhaseKind::Waiting
            && let Some(reason) = reason
        {
            let room = usize::from(area.width).saturating_sub(Line::from(spans.clone()).width());
            let text = fit(&reason.content, room).trim_end().to_owned();
            spans.push(Span::styled(text, reason.style));
        }
    }
    frame.render_widget(Line::from(spans), legend);
}

/// The same job's latest runs, oldest first, as the dashboard's job page compares them: a heading with the median
/// run and wait, the failures and the largest memory peak, then a bar each of their run times in their outcome's
/// colour, their waits and their memory peaks in the profile's colour, this run reversed in each.
fn job_runs(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    summary: &JobSummary,
    compared: &RunComparison,
    now: u64,
    theme: Theme,
) {
    let [title, bars] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    let runs = &compared.runs;
    let peak = compared.largest_peak;
    let mut note = vec![];
    if let Some(ran) = compared.median_ran {
        note.push((0, format!("median run {}", duration(ran))));
    }
    if let Some(waited) = compared.median_waited {
        note.push((1, format!("wait {}", duration(waited))));
    }
    note.push((
        0,
        format!("{} of {} failed", compared.failed, compared.finished),
    ));
    if let Some(peak) = peak {
        note.push((2, format!("largest peak {}", bytes(peak))));
    }
    let heading_title = format!("Latest {} runs", runs.len());
    let room = usize::from(title.width).saturating_sub(heading_title.width() + 2);
    heading(frame, title, &heading_title, fit_details(note, room), theme);
    // Three bars of equal width, each after its label, showing the newest runs that fit.
    let labels = ["ran ", "  waited ", "  peak "];
    let label_width: usize = labels.iter().map(|label| label.width()).sum();
    let width = (usize::from(bars.width).saturating_sub(label_width) / 3).min(runs.len());
    let shown = &runs[runs.len() - width..];
    let longest = shown
        .iter()
        .filter_map(|run| run.ran(now))
        .max()
        .unwrap_or(0);
    let longest_wait = shown
        .iter()
        .filter_map(|run| run.waited(now))
        .max()
        .unwrap_or(0);
    let profile = theme.profile(
        summary
            .runner
            .as_ref()
            .and_then(|runner| app.profile_index(&runner.profile)),
    );
    let this = |run: &JobSummary, style: Style| {
        if run.id == summary.id {
            style.add_modifier(Modifier::REVERSED)
        } else {
            style
        }
    };
    let bar = |value: Option<u64>, max: u64| {
        spark(value.into_iter(), max, 1)
            .chars()
            .next()
            .unwrap_or(' ')
    };
    let mut spans = vec![Span::styled(labels[0], theme.dim())];
    spans.extend(shown.iter().map(|run| {
        Span::styled(
            bar(run.ran(now), longest).to_string(),
            this(run, theme.tone(human::outcome(run).1)),
        )
    }));
    spans.push(Span::styled(labels[1], theme.dim()));
    spans.extend(shown.iter().map(|run| {
        Span::styled(
            bar(run.waited(now), longest_wait).to_string(),
            this(run, theme.wait()),
        )
    }));
    spans.push(Span::styled(labels[2], theme.dim()));
    spans.extend(shown.iter().map(|run| {
        Span::styled(
            bar(run.metrics.memory_peak, peak.unwrap_or(0)).to_string(),
            this(run, profile),
        )
    }));
    frame.render_widget(Line::from(spans), bars);
}

/// Each step as a bar on the job's time axis, with its duration. Steps that do not fit are left out around the
/// one in progress.
fn steps(frame: &mut Frame<'_>, area: Rect, job: &Job, now: u64, theme: Theme) {
    if area.height == 0 {
        return;
    }
    let [title, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let start = job
        .summary
        .timeline
        .started_at
        .or_else(|| job.steps.iter().filter_map(|step| step.started_at).min());
    let end = job.summary.timeline.finished_at.unwrap_or_else(|| {
        if job.summary.state.is_final() {
            job.steps
                .iter()
                .filter_map(|step| step.completed_at)
                .max()
                .unwrap_or(now)
        } else {
            now
        }
    });
    heading(
        frame,
        title,
        "Steps",
        start.map_or_else(String::new, |start| {
            format!(
                "{} in {} steps",
                duration(end.saturating_sub(start)),
                job.steps.len()
            )
        }),
        theme,
    );
    let Some(start) = start else {
        return;
    };
    let span = end.saturating_sub(start).max(1);
    let shown = usize::from(rest.height);
    let current = watch::current_step(job).map_or(0, |(_, position)| position);
    let first = current
        .saturating_sub(shown / 2)
        .min(job.steps.len().saturating_sub(shown));
    let name_width = job
        .steps
        .iter()
        .map(|step| step.name.width())
        .max()
        .unwrap_or(0)
        .min(usize::from(rest.width) / 3)
        .max(8);
    let bar_width = rest
        .width
        .saturating_sub(u16::try_from(name_width).unwrap_or(u16::MAX) + 2 + 10);
    let lines: Vec<Line<'_>> = job
        .steps
        .iter()
        .skip(first)
        .take(shown)
        .map(|step| step_line(step, start, span, now, name_width, bar_width, theme))
        .collect();
    frame.render_widget(Paragraph::new(lines), rest);
}

fn step_line(
    step: &Step,
    start: u64,
    span: u64,
    now: u64,
    name_width: usize,
    bar_width: u16,
    theme: Theme,
) -> Line<'static> {
    let running = step.is_running();
    let name_style = match (step.started_at, running) {
        (_, true) => bold(),
        (None, _) => theme.dim(),
        _ => Style::new(),
    };
    let mut spans = vec![Span::styled(
        format!("{} ", fit(&step.name, name_width)),
        name_style,
    )];
    let Some(started) = step.started_at else {
        spans.push(Span::raw(" ".repeat(usize::from(bar_width))));
        spans.push(Span::styled(format!(" {:>9}", "–"), theme.dim()));
        return Line::from(spans);
    };
    // A step that ended without a completion time has no duration to show; its bar is a mark at its start.
    let took = step.took(now);
    let offset = scale(started.saturating_sub(start), span, bar_width);
    let length = scale(took.unwrap_or(0), span, bar_width)
        .max(1)
        .min(bar_width.saturating_sub(offset));
    let style = match step.outcome().kind {
        api::OutcomeKind::Succeeded => theme.memory(),
        api::OutcomeKind::Failed => theme.tone(Tone::Bad),
        api::OutcomeKind::Running => theme.cpu(),
        api::OutcomeKind::Waiting | api::OutcomeKind::Ended => theme.dim(),
    };
    let glyph = if running { "▓" } else { "█" };
    spans.extend([
        Span::raw(" ".repeat(usize::from(offset))),
        Span::styled(glyph.repeat(usize::from(length)), style),
        Span::raw(" ".repeat(usize::from(bar_width - offset - length))),
        Span::styled(
            format!(
                " {:>9}",
                took.map_or_else(
                    || "–".to_owned(),
                    |took| format!("{}{}", duration(took), if running { "…" } else { "" })
                )
            ),
            bold(),
        ),
    ]);
    Line::from(spans)
}

/// The open job's chart lines, as `Job::chart_lines` labels them for every client.
fn chart_lines(app: &App, job: &Job) -> ChartLines {
    let profile = job.summary.runner.as_ref().and_then(|runner| {
        app.live
            .status
            .profiles
            .iter()
            .find(|profile| profile.name == runner.profile)
    });
    job.chart_lines(profile)
}

/// The highest of a chart's lines, which tops it.
fn highest(lines: &[ChartLine]) -> u64 {
    lines.iter().map(|line| line.value).max().unwrap_or(0)
}

/// A job's chart of `values` with its lines: the cap on top, the reservation under it.
fn job_chart(
    title: &'static str,
    lines: &[ChartLine],
    values: Vec<u64>,
    unit: fn(u64) -> String,
    style: Style,
) -> Chart {
    let labels = |reserved: bool| {
        lines
            .iter()
            .filter(|line| (line.kind == ChartLineKind::Reserved) == reserved)
            .map(|line| line.label.as_str())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    Chart {
        title,
        limit_label: labels(false),
        reserved_label: labels(true),
        values,
        limit: highest(lines),
        unit,
        style,
    }
}

/// The job's figures that fit `width`, in order; the rest go whole.
fn metrics_line(open: &JobView, width: usize, theme: Theme) -> Line<'static> {
    let metrics = &open.job.summary.metrics;
    let peak_memory = metrics
        .memory_peak
        .or_else(|| open.samples.iter().map(|sample| sample.memory).max());
    let peak_cpu = metrics
        .cpu_peak
        .or_else(|| open.samples.iter().map(|sample| sample.cpu).max());
    let mut parts = vec![
        ("mem peak", peak_memory.map(bytes)),
        ("avg", metrics.memory_average.map(bytes)),
        ("cpu peak", peak_cpu.map(cores_used)),
        ("avg", metrics.cpu_average.map(cores_used)),
        ("cpu time", metrics.cpu_time.map(api::cpu_time)),
        ("in", metrics.network_received.map(bytes)),
        ("out", metrics.network_sent.map(bytes)),
    ];
    if metrics.mirror_bytes > 0 {
        parts.push(("mirror", Some(bytes(metrics.mirror_bytes))));
    }
    if let Some(activity) = metrics.build_cache.activity() {
        let what = match activity.counts.hit_percent() {
            Some(percent) => format!("{percent}% hit"),
            None => activity
                .refused_line()
                .unwrap_or_else(|| activity.stored_line()),
        };
        parts.push(("build cache", Some(what)));
    }
    let mut line = Line::default();
    for (label, value) in parts {
        let Some(value) = value else { continue };
        let gap = if line.spans.is_empty() { "" } else { "  " };
        if line.width() + gap.width() + label.width() + 1 + value.width() > width {
            break;
        }
        line.push_span(Span::styled(format!("{gap}{label} "), theme.dim()));
        line.push_span(Span::styled(value, bold()));
    }
    line
}

fn job_charts(frame: &mut Frame<'_>, area: Rect, app: &App, open: &JobView, theme: Theme) {
    let [charts, metrics] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let [memory_area, cpu_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(charts);
    let lines = chart_lines(app, &open.job);
    job_chart(
        "Memory",
        &lines.memory,
        open.samples.iter().map(|sample| sample.memory).collect(),
        bytes,
        theme.memory(),
    )
    .render(frame, memory_area, theme);
    job_chart(
        "CPU",
        &lines.cpu,
        open.samples.iter().map(|sample| sample.cpu).collect(),
        in_cores,
        theme.cpu(),
    )
    .render(frame, cpu_area, theme);
    frame.render_widget(
        metrics_line(open, usize::from(metrics.width), theme),
        metrics,
    );
}

/// Memory and CPU as one line of sparklines, for a terminal too small for charts.
fn job_spark(frame: &mut Frame<'_>, area: Rect, app: &App, open: &JobView, theme: Theme) {
    let [sparks, metrics] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    let lines = chart_lines(app, &open.job);
    let (memory_cap, cpu_cap) = (highest(&lines.memory), highest(&lines.cpu));
    let width = usize::from(area.width / 2).saturating_sub(16);
    let last = open.samples.back();
    frame.render_widget(
        Line::from(vec![
            Span::styled("mem ", theme.dim()),
            Span::raw(format!(
                "{:>6} ",
                last.map_or_else(String::new, |s| bytes(s.memory))
            )),
            Span::styled(
                format!(
                    "{:<width$} ",
                    spark(open.samples.iter().map(|s| s.memory), memory_cap, width)
                ),
                theme.memory(),
            ),
            Span::styled("cpu ", theme.dim()),
            Span::raw(format!(
                "{:>5} ",
                last.map_or_else(String::new, |s| cores_used(s.cpu))
            )),
            Span::styled(
                spark(open.samples.iter().map(|s| s.cpu), cpu_cap, width),
                theme.cpu(),
            ),
        ]),
        sparks,
    );
    frame.render_widget(
        metrics_line(open, usize::from(metrics.width), theme),
        metrics,
    );
}

fn log(frame: &mut Frame<'_>, area: Rect, open: &JobView, theme: Theme) {
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
fn without_timestamp(line: &str) -> &str {
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
