//! What `runnerctl tui` draws: the overview (budget, slots, the slice, repositories, latest jobs), the history and
//! one job (timeline, steps on one time axis, memory and CPU, its log). Reads `App` and the time only; panes that do
//! not fit a small terminal are left out or drawn one line each. Colours follow the web dashboard: profiles in
//! config order take its palette, mint first and pink second; without colour, glyphs and bold carry the meaning.

use std::fmt::Write as _;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState,
};
use runnerd_core::api::{Job, JobSummary, ProfileStatus, Sample, SlotStatus, Step};

use super::app::{App, Connection, JobView, Outcome, Pane, View};
use crate::human::{self, Tone, bytes, cores, duration};
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

    fn tone(self, tone: Tone) -> Style {
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
    let mut spans = vec![
        Span::styled(" runnerd ", bold()),
        Span::styled(format!("{}  ", app.live.status.version), theme.dim()),
    ];
    for (key, name, view) in [
        ("1", "Overview", View::Overview),
        ("2", "History", View::History),
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
    frame.render_widget(Line::from(spans), area);
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
    frame.render_widget(Line::from(right).alignment(Alignment::Right), area);
}

fn footer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let line = if let Some((_, message)) = &app.confirm {
        Line::from(vec![
            Span::styled(
                " y cancels its run and pauses, any other key keeps it running: ",
                bold(),
            ),
            Span::styled(message.clone(), theme.wait()),
        ])
    } else if let Some(flash) = &app.flash {
        let style = if flash.outcome == Outcome::Failed {
            theme.tone(Tone::Bad)
        } else {
            theme.memory()
        };
        Line::styled(format!(" {}", flash.text), style)
    } else {
        let keys = match app.view {
            View::Overview => {
                "tab pane  j/k move  enter open  p pause  r resume  R reseed  ? help  q quit"
            }
            View::History => {
                "j/k move  enter open  c outcome  s repository  ] next  [ back  ? help  q quit"
            }
            View::Job => {
                "j/k scroll  G follow  p pause  r resume  R reseed  esc back  ? help  q quit"
            }
        };
        Line::styled(format!(" {keys}"), theme.dim())
    };
    frame.render_widget(line, area);
}

fn help(frame: &mut Frame<'_>, area: Rect) {
    const KEYS: [(&str, &str); 17] = [
        ("1 o", "overview"),
        ("2 h", "history"),
        ("tab", "next pane of the overview"),
        ("j k ↓ ↑", "move, or scroll the log"),
        ("PgDn PgUp", "move by ten"),
        ("enter", "open the job"),
        ("esc", "back from a job"),
        ("G g", "follow the log, or go to its start"),
        ("p", "pause the repository: no new runners"),
        ("r", "resume the repository"),
        ("R", "reseed the profile's snapshot"),
        ("c", "history: cycle the outcome filter"),
        ("s", "history: cycle the repository filter"),
        ("] [", "history: next and previous page"),
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
fn heading(frame: &mut Frame<'_>, area: Rect, title: &str, note: String, theme: Theme) {
    frame.render_widget(Line::styled(title.to_owned(), bold()), area);
    frame.render_widget(
        Line::styled(note, theme.dim()).alignment(Alignment::Right),
        area,
    );
}

fn overview(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let slots = app.slots().count();
    let columns = usize::from(area.width / 30).clamp(1, slots.max(1));
    let cards = u16::try_from(slots.div_ceil(columns) * 7 + 1).unwrap_or(u16::MAX);
    let lines = u16::try_from(slots + 1).unwrap_or(u16::MAX);
    let rest = area.height.saturating_sub(3);
    let as_cards = rest >= cards + 8;
    let slots_height = if as_cards { cards } else { lines };
    let charts = rest.saturating_sub(slots_height) >= 7 + 6;
    let [budget_area, slots_area, charts_area, lists_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(slots_height),
        Constraint::Length(if charts { 7 } else { 0 }),
        Constraint::Min(0),
    ])
    .areas(area);
    budget(frame, budget_area, app, theme);
    if as_cards {
        slot_cards(frame, slots_area, app, columns, now, theme);
    } else {
        slot_lines(frame, slots_area, app, now, theme);
    }
    if charts {
        slice(frame, charts_area, app, theme);
    }
    let [repositories_area, latest_area] = if lists_area.width >= 70 {
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .areas(lists_area)
    } else {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(lists_area)
    };
    repositories(frame, repositories_area, app, theme);
    latest(frame, latest_area, app, now, theme);
}

/// `part` of `whole` as cells of `width`.
fn scale(part: u64, whole: u64, width: u16) -> u16 {
    let cells = u128::from(part) * u128::from(width) / u128::from(whole.max(1));
    u16::try_from(cells).unwrap_or(width).min(width)
}

/// The budget as one bar: a block per slot as wide as its memory, filled as far as its job uses it; then what
/// else is reserved (the mirror, seed containers), then what is free.
fn budget(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let status = &app.live.status;
    let budget = &status.budget;
    let mut note = match budget.memory_max {
        Some(max) => format!(
            "{} of {} reserved in {}",
            bytes(budget.memory_in_use),
            bytes(max),
            budget.slice
        ),
        None => format!(
            "{} reserved in {}, no MemoryMax",
            bytes(budget.memory_in_use),
            budget.slice
        ),
    };
    if let Some(host) = &app.live.host {
        let _ = write!(note, " · {} in use, sampled", bytes(host.memory));
    }
    let [title, bar, labels] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    heading(frame, title, "Budget", note, theme);

    let caps: u64 = app.slots().map(|(_, profile, _)| profile.memory).sum();
    let whole = budget.memory_max.unwrap_or(caps).max(caps);
    let mut cells = Vec::new();
    let mut names = Vec::new();
    let mut reserved = 0;
    let mut drawn = 0;
    for (index, profile, slot) in app.slots() {
        let width = scale(profile.memory, whole, bar.width).max(1);
        let style = theme.profile(Some(index));
        let sample =
            slot_job(app, profile, slot).and_then(|job| app.live.samples.get(&job.summary.id));
        let used = sample.map_or(0, |sample| scale(sample.memory, profile.memory, width));
        let (text, label) = if slot.runner.is_some() {
            reserved += profile.memory;
            (
                format!(
                    "{}{}",
                    "█".repeat(usize::from(used)),
                    "▓".repeat(usize::from(width - used))
                ),
                sample.map_or_else(
                    || format!("{} {} {}", profile.name, slot.slot, bytes(profile.memory)),
                    |sample| {
                        format!(
                            "{} {} {} used",
                            profile.name,
                            slot.slot,
                            bytes(sample.memory)
                        )
                    },
                ),
            )
        } else {
            (
                "░".repeat(usize::from(width)),
                format!("{} {} idle", profile.name, slot.slot),
            )
        };
        // A gap between blocks, taken from the block itself.
        let mut text: String = text
            .chars()
            .take(usize::from(width.saturating_sub(1)))
            .collect();
        text.push(' ');
        cells.push(Span::styled(text, style));
        names.push(Span::styled(
            format!("{} ", fit(&label, usize::from(width.saturating_sub(1)))),
            style,
        ));
        drawn += width;
    }
    let other = scale(
        budget.memory_in_use.saturating_sub(reserved),
        whole,
        bar.width,
    )
    .min(bar.width.saturating_sub(drawn));
    cells.push(Span::raw("▒".repeat(usize::from(other))));
    let free = bar.width.saturating_sub(drawn + other);
    cells.push(Span::styled("·".repeat(usize::from(free)), theme.dim()));
    frame.render_widget(Line::from(cells), bar);
    frame.render_widget(Line::from(names), labels);
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

/// `text` cut or padded to `width` characters, ending in `…` when cut.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return format!("{text:<width$}");
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    if width > 0 {
        cut.push('…');
    }
    cut
}

/// What a slot's job shows: memory and CPU now, their caps and their recent samples.
struct Usage<'a> {
    samples: Vec<&'a Sample>,
    memory_cap: u64,
    cpu_cap: u64,
}

impl Usage<'_> {
    fn of<'a>(app: &'a App, profile: &ProfileStatus, job: &Job) -> Usage<'a> {
        let runner = job.summary.runner.as_ref();
        Usage {
            samples: app
                .series
                .get(&job.summary.id)
                .map(|series| series.iter().collect())
                .unwrap_or_default(),
            memory_cap: runner
                .and_then(|runner| runner.memory_cap)
                .unwrap_or(profile.memory),
            cpu_cap: runner
                .and_then(|runner| runner.cpu_cap)
                .unwrap_or(u64::from(profile.cpus) * 1000),
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

/// `3/9 Run tests`: the step a job is in.
fn step_label(job: &Job) -> Option<(String, String)> {
    watch::current_step(job)
        .map(|(step, position)| (format!("{position}/{}", job.steps.len()), step.name.clone()))
}

fn slot_lines(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let running = app.live.running.len();
    let queued = app.live.queued.len();
    let [title, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    heading(
        frame,
        title,
        "Slots",
        format!("{running} running · {queued} queued"),
        theme,
    );
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
            match (slot_job(app, profile, slot), &slot.runner) {
                (Some(job), _) => {
                    let usage = Usage::of(app, profile, job);
                    let (count, name) = step_label(job).unwrap_or_default();
                    let ran = job
                        .summary
                        .timeline
                        .started_at
                        .map_or_else(String::new, |at| duration(now.saturating_sub(at)));
                    let memory = usage.last().map_or_else(String::new, |s| bytes(s.memory));
                    let cpu = usage.last().map_or_else(String::new, |s| cores(s.cpu));
                    // Fixed columns at the right; the job's name takes what is left.
                    let job_width = width.saturating_sub(11 + 22 + 8 + 13 + 11).max(10);
                    spans.extend([
                        Span::raw(fit(
                            &format!("{} {}", human::title(&job.summary), job.summary.repository),
                            job_width,
                        )),
                        Span::styled(format!(" {count:>5} "), theme.cpu()),
                        Span::raw(fit(&name, 16)),
                        Span::raw(format!("{ran:>7} ")),
                        Span::raw(format!("{memory:>6} ")),
                        Span::styled(usage.memory(6), theme.memory()),
                        Span::raw(format!("{cpu:>5} ")),
                        Span::styled(usage.cpu(6), theme.cpu()),
                    ]);
                }
                (None, Some(runner)) => spans.push(Span::styled(
                    format!("runner for {} waiting for its job", runner.repository),
                    theme.dim(),
                )),
                (None, None) => spans.push(Span::styled("idle", theme.dim())),
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
    heading(
        frame,
        title,
        "Slots",
        format!(
            "{} running · {} queued",
            app.live.running.len(),
            app.live.queued.len()
        ),
        theme,
    );
    let slots: Vec<_> = app.slots().collect();
    let rows =
        Layout::vertical(vec![Constraint::Length(7); slots.len().div_ceil(columns)]).split(rest);
    for (position, (index, profile, slot)) in slots.into_iter().enumerate() {
        let row = rows[position / columns];
        let cells = Layout::horizontal(vec![
            Constraint::Ratio(
                1,
                u32::try_from(columns).unwrap_or(1)
            );
            columns
        ])
        .split(row);
        let cell = cells[position % columns];
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
                    format!("{} · slot {} ", profile.name, slot.slot),
                    if focused { selected() } else { bold() },
                ),
            ]))
            .title_top(
                Line::styled(
                    format!(" {} · {} CPUs ", bytes(profile.memory), profile.cpus),
                    theme.dim(),
                )
                .right_aligned(),
            );
        let inner = block.inner(cell);
        frame.render_widget(block, cell);
        let lines = card(app, profile, slot, usize::from(inner.width), now, theme);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// What a slot card shows: its job with step, time, memory and CPU; its runner waiting for a job; or nothing.
fn card(
    app: &App,
    profile: &ProfileStatus,
    slot: &SlotStatus,
    width: usize,
    now: u64,
    theme: Theme,
) -> Vec<Line<'static>> {
    match (slot_job(app, profile, slot), &slot.runner) {
        (Some(job), _) => {
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
                .map_or_else(|| "–".to_owned(), |s| cores(s.cpu));
            let half = width.saturating_sub(1) / 2;
            vec![
                Line::styled(job.summary.repository.clone(), theme.dim()),
                Line::styled(human::title(&job.summary), bold()),
                Line::from(vec![
                    Span::styled(
                        format!("{count} "),
                        theme.cpu().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(name),
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
        (None, Some(runner)) => vec![
            Line::styled(
                format!("Runner for {} waiting for its job", runner.repository),
                theme.dim(),
            ),
            Line::styled(
                format!("started {}", human::ago(runner.started_at, now)),
                theme.dim(),
            ),
        ],
        (None, None) => vec![Line::styled("Idle", theme.dim())],
    }
}

/// The slice's memory and CPU, every runner container together, as filled charts against their limits.
fn slice(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let [memory_area, cpu_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let status = &app.live.status;
    let memory_max = status.budget.memory_max;
    let cpus: u64 = status
        .profiles
        .iter()
        .map(|profile| u64::from(profile.cpus) * profile.slots.len() as u64)
        .sum();
    Chart {
        title: "Memory",
        limit_label: memory_max.map_or_else(String::new, |max| format!("MemoryMax {}", bytes(max))),
        values: app.host.iter().map(|sample| sample.memory).collect(),
        limit: memory_max.unwrap_or(0),
        unit: bytes,
        style: theme.memory(),
    }
    .render(frame, memory_area, theme);
    Chart {
        title: "CPU",
        limit_label: format!("{cpus} CPUs of all slots"),
        values: app.host.iter().map(|sample| sample.cpu).collect(),
        limit: cpus * 1000,
        unit: in_cores,
        style: theme.cpu(),
    }
    .render(frame, cpu_area, theme);
}

/// A filled chart of the newest values that fit, scaled to `limit` (or to the values' peak when there is none),
/// with the last value in its title and the peak under it.
struct Chart<'a> {
    title: &'a str,
    limit_label: String,
    values: Vec<u64>,
    limit: u64,
    unit: fn(u64) -> String,
    style: Style,
}

impl Chart<'_> {
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
            .title_top(Line::styled(format!(" {} ", self.limit_label), theme.dim()).right_aligned())
            .title_bottom(Line::styled(
                peak.map_or_else(
                    || " no samples yet ".to_owned(),
                    |peak| format!(" sampled peak {} ", unit(peak)),
                ),
                theme.dim(),
            ));
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

fn in_cores(millicores: u64) -> String {
    format!("{} cores", cores(millicores))
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

fn repositories(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let focused = app.pane == Pane::Repositories;
    let lines: Vec<Line<'_>> = app
        .live
        .status
        .repositories
        .iter()
        .enumerate()
        .map(|(position, repository)| {
            let mut spans = vec![
                Span::styled("■ ", theme.profile(app.profile_index(&repository.profile))),
                Span::styled(repository.name.clone(), bold()),
                Span::styled(format!("  {}", repository.profile), theme.dim()),
                Span::raw(format!("  {} queued", repository.queued_jobs)),
            ];
            if repository.paused {
                spans.push(Span::styled(
                    "  paused",
                    theme.wait().add_modifier(Modifier::BOLD),
                ));
            }
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

/// An outcome as a short upper-case badge.
fn badge(job: &JobSummary, theme: Theme) -> Span<'static> {
    let (word, tone) = human::outcome(job);
    Span::styled(
        format!("{:<9}", word.to_uppercase()),
        theme.tone(tone).add_modifier(Modifier::BOLD),
    )
}

fn latest(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let focused = app.pane == Pane::Latest;
    let width = usize::from(area.width);
    let lines: Vec<Line<'_>> = app
        .live
        .recent
        .iter()
        .enumerate()
        .map(|(position, job)| {
            let ran = human::ran(job, now).map_or_else(String::new, duration);
            let ago = job
                .timeline
                .finished_at
                .map_or_else(String::new, |at| human::ago(at, now));
            let name_width = width.saturating_sub(10 + 9 + 10).max(8);
            let line = Line::from(vec![
                badge(job, theme),
                Span::raw(" "),
                Span::raw(fit(
                    &format!("{} {}", human::title(job), job.repository),
                    name_width,
                )),
                Span::raw(format!("{ran:>8} ")),
                Span::styled(format!("{ago:>9}"), theme.dim()),
            ]);
            if focused && position == app.latest {
                line.style(selected())
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(list_block("Latest jobs", focused, theme)),
        area,
    );
}

fn history(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let filters = &app.history;
    let [title, table_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let page = filters.previous.len() + 1;
    heading(
        frame,
        title,
        "History",
        format!(
            "outcome {} · repository {} · page {page}",
            filters.conclusion.as_deref().unwrap_or("any"),
            filters.repository.as_deref().unwrap_or("any"),
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
    let rows = jobs.jobs.iter().map(|job| {
        let mut cells = vec![
            Cell::from(badge(job, theme)),
            Cell::from(job.repository.clone()),
            Cell::from(human::title(job)),
        ];
        if wide {
            cells.push(Cell::from(job.branch.clone().unwrap_or_default()));
        }
        cells.extend([
            Cell::from(human::waited(job, now).map_or_else(String::new, duration)),
            Cell::from(human::ran(job, now).map_or_else(String::new, duration)),
            Cell::from(job.metrics.memory_peak.map_or_else(String::new, bytes)),
        ]);
        if wide {
            cells.push(Cell::from(
                job.metrics.cpu_average.map_or_else(String::new, cores),
            ));
        }
        cells.push(Cell::from(Span::styled(
            job.timeline
                .finished_at
                .map_or_else(String::new, |at| human::ago(at, now)),
            theme.dim(),
        )));
        Row::new(cells)
    });
    let mut header = vec!["OUTCOME", "REPOSITORY", "JOB"];
    let mut widths = vec![
        Constraint::Length(9),
        Constraint::Fill(1),
        Constraint::Fill(1),
    ];
    if wide {
        header.push("BRANCH");
        widths.push(Constraint::Length(14));
    }
    header.extend(["WAITED", "RAN", "MEM"]);
    widths.extend([
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(6),
    ]);
    if wide {
        header.push("CPU");
        widths.push(Constraint::Length(5));
    }
    header.push("FINISHED");
    widths.push(Constraint::Length(10));
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(theme.dim()))
        .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(filters.selected));
    frame.render_stateful_widget(table, table_area, &mut state);
}

fn job(frame: &mut Frame<'_>, area: Rect, app: &App, open: &JobView, now: u64, theme: Theme) {
    let job = &open.job;
    let summary = &job.summary;
    let steps_wanted = u16::try_from(job.steps.len() + 1).unwrap_or(u16::MAX);
    let fixed = 3 + 2;
    let full_charts = area.height >= fixed + steps_wanted + 7 + 6;
    let charts_height = if full_charts { 7 } else { 2 };
    let steps_height = steps_wanted.min(area.height.saturating_sub(fixed + charts_height + 4));
    let [head, timeline_area, steps_area, charts_area, log_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Length(steps_height),
        Constraint::Length(charts_height),
        Constraint::Min(0),
    ])
    .areas(area);

    let profile = summary.runner.as_ref().and_then(|runner| {
        app.live
            .status
            .profiles
            .iter()
            .find(|p| p.name == runner.profile)
    });
    let mut details = vec![];
    if let Some(branch) = &summary.branch {
        details.push(format!("branch {branch}"));
    }
    details.push(format!("run {}", summary.run_id));
    if let Some(runner) = &summary.runner {
        details.push(format!("slot {} · {}", runner.profile, runner.slot));
        match (runner.memory_cap, runner.cpu_cap, profile) {
            (Some(memory), Some(cpu), _) => {
                details.push(format!(
                    "capped at {} and {} cores",
                    bytes(memory),
                    cores(cpu)
                ));
            }
            (_, _, Some(profile)) => details.push(format!(
                "profile now {} and {} CPUs",
                bytes(profile.memory),
                profile.cpus
            )),
            _ => {}
        }
    }
    if let Some(waited) = human::waited(summary, now) {
        details.push(format!("waited {}", duration(waited)));
    }
    if let Some(ran) = human::ran(summary, now) {
        details.push(format!("ran {}", duration(ran)));
    }
    let style = summary.runner.as_ref().map_or_else(
        || theme.dim(),
        |runner| theme.profile(app.profile_index(&runner.profile)),
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("■ ", style),
                Span::styled(
                    format!(
                        "{}{}",
                        summary.repository,
                        summary
                            .workflow
                            .as_ref()
                            .map_or_else(String::new, |workflow| format!(" · {workflow}"))
                    ),
                    theme.dim(),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    format!(
                        "{}  ",
                        if summary.name.is_empty() {
                            human::title(summary)
                        } else {
                            summary.name.clone()
                        }
                    ),
                    bold(),
                ),
                badge(summary, theme),
            ]),
            Line::styled(details.join(" · "), theme.dim()),
        ]),
        head,
    );
    timeline(frame, timeline_area, summary, now, theme);
    steps(frame, steps_area, job, now, theme);
    if full_charts {
        job_charts(frame, charts_area, app, open, theme);
    } else {
        job_spark(frame, charts_area, app, open, theme);
    }
    log(frame, log_area, open, theme);
}

/// Waiting for a runner, the runner starting, and running, as one bar.
fn timeline(frame: &mut Frame<'_>, area: Rect, job: &JobSummary, now: u64, theme: Theme) {
    let timeline = &job.timeline;
    let end = timeline.finished_at.unwrap_or(now);
    let started = timeline.started_at;
    let container = timeline.container_started_at.or(started);
    let waiting = container.unwrap_or(end).saturating_sub(timeline.queued_at);
    let starting = match (container, started) {
        (Some(container), Some(started)) => started.saturating_sub(container),
        _ => 0,
    };
    let running = started.map_or(0, |started| end.saturating_sub(started));
    let whole = waiting + starting + running;
    let width = area.width;
    let starting_cells = scale(starting, whole, width).max(u16::from(starting > 0));
    let waiting_cells = scale(waiting, whole, width).max(u16::from(waiting > 0));
    let running_cells = width.saturating_sub(waiting_cells + starting_cells);
    let still = if timeline.finished_at.is_none() {
        "…"
    } else {
        ""
    };
    let [bar, legend] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    frame.render_widget(
        Line::from(vec![
            Span::styled("░".repeat(usize::from(waiting_cells)), theme.wait()),
            Span::styled("▒".repeat(usize::from(starting_cells)), theme.cpu()),
            Span::styled(
                "█".repeat(usize::from(if started.is_some() {
                    running_cells
                } else {
                    0
                })),
                theme.memory(),
            ),
        ]),
        bar,
    );
    let mut spans = vec![
        Span::styled("░ ", theme.wait()),
        Span::raw(format!("waiting for a runner {}  ", duration(waiting))),
    ];
    if started.is_some() {
        spans.extend([
            Span::styled("▒ ", theme.cpu()),
            Span::raw(format!("runner starting {}  ", duration(starting))),
            Span::styled("█ ", theme.memory()),
            Span::raw(format!("running {}{still}", duration(running))),
        ]);
    }
    frame.render_widget(Line::from(spans), legend);
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
        .map(|step| step.name.chars().count())
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
    let running = step.status == "in_progress";
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
    let finished = step
        .completed_at
        .unwrap_or(if running { now } else { started });
    let offset = scale(started.saturating_sub(start), span, bar_width);
    let length = scale(finished.saturating_sub(started), span, bar_width)
        .max(1)
        .min(bar_width.saturating_sub(offset));
    let style = if running {
        theme.cpu()
    } else if matches!(step.conclusion.as_deref(), Some("failure" | "cancelled")) {
        theme.tone(Tone::Bad)
    } else if step.conclusion.as_deref() == Some("skipped") {
        theme.dim()
    } else {
        theme.memory()
    };
    let glyph = if running { "▓" } else { "█" };
    spans.extend([
        Span::raw(" ".repeat(usize::from(offset))),
        Span::styled(glyph.repeat(usize::from(length)), style),
        Span::raw(" ".repeat(usize::from(bar_width - offset - length))),
        Span::styled(
            format!(
                " {:>9}",
                format!(
                    "{}{}",
                    duration(finished.saturating_sub(started)),
                    if running { "…" } else { "" }
                )
            ),
            bold(),
        ),
    ]);
    Line::from(spans)
}

/// The open job's caps: its container's, or the profile's current ones for a record from before caps were kept.
fn caps(app: &App, job: &JobSummary) -> (u64, u64) {
    let runner = job.runner.as_ref();
    let profile = runner.and_then(|runner| {
        app.live
            .status
            .profiles
            .iter()
            .find(|profile| profile.name == runner.profile)
    });
    (
        runner
            .and_then(|runner| runner.memory_cap)
            .or(profile.map(|profile| profile.memory))
            .unwrap_or(0),
        runner
            .and_then(|runner| runner.cpu_cap)
            .or(profile.map(|profile| u64::from(profile.cpus) * 1000))
            .unwrap_or(0),
    )
}

fn metrics_line(open: &JobView, theme: Theme) -> Line<'static> {
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
        ("cpu peak", peak_cpu.map(cores)),
        ("avg", metrics.cpu_average.map(cores)),
        ("cpu time", metrics.cpu_time.map(human::cpu_time)),
        ("in", metrics.network_received.map(bytes)),
        ("out", metrics.network_sent.map(bytes)),
    ];
    if metrics.mirror_bytes > 0 {
        parts.push(("mirror", Some(bytes(metrics.mirror_bytes))));
    }
    let mut spans = Vec::new();
    for (label, value) in parts {
        if let Some(value) = value {
            spans.push(Span::styled(format!("{label} "), theme.dim()));
            spans.push(Span::styled(format!("{value}  "), bold()));
        }
    }
    Line::from(spans)
}

fn job_charts(frame: &mut Frame<'_>, area: Rect, app: &App, open: &JobView, theme: Theme) {
    let [charts, metrics] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let [memory_area, cpu_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(charts);
    let (memory_cap, cpu_cap) = caps(app, &open.job.summary);
    Chart {
        title: "Memory",
        limit_label: format!("cap {}", bytes(memory_cap)),
        values: open.samples.iter().map(|sample| sample.memory).collect(),
        limit: memory_cap,
        unit: bytes,
        style: theme.memory(),
    }
    .render(frame, memory_area, theme);
    Chart {
        title: "CPU",
        limit_label: format!("cap {}", in_cores(cpu_cap)),
        values: open.samples.iter().map(|sample| sample.cpu).collect(),
        limit: cpu_cap,
        unit: in_cores,
        style: theme.cpu(),
    }
    .render(frame, cpu_area, theme);
    frame.render_widget(metrics_line(open, theme), metrics);
}

/// Memory and CPU as one line of sparklines, for a terminal too small for charts.
fn job_spark(frame: &mut Frame<'_>, area: Rect, app: &App, open: &JobView, theme: Theme) {
    let [sparks, metrics] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    let (memory_cap, cpu_cap) = caps(app, &open.job.summary);
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
                last.map_or_else(String::new, |s| cores(s.cpu))
            )),
            Span::styled(
                spark(open.samples.iter().map(|s| s.cpu), cpu_cap, width),
                theme.cpu(),
            ),
        ]),
        sparks,
    );
    frame.render_widget(metrics_line(open, theme), metrics);
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
                    " {} lines, {}{} · {following} ",
                    summary.lines.max(open.log.len() as u64),
                    bytes(summary.bytes),
                    if summary.truncated { ", cut" } else { "" }
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
