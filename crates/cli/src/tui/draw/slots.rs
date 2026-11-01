//! The overview's slots, as cards or as one line each.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use runnerd_core::api::{Job, ProfileStatus, Sample, SlotStatus, Status};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::{bold, fit, heading, selected, spark};
use crate::human::{self, bytes, cores_used, duration};
use crate::tui::app::{App, Pane};
use crate::watch;

/// The running job in a slot.
pub(super) fn slot_job<'a>(
    app: &'a App,
    profile: &ProfileStatus,
    slot: &SlotStatus,
) -> Option<&'a Job> {
    app.live.job_in(&profile.name, slot.slot)
}

/// What a slot's job shows: memory and CPU now, their caps and their recent samples.
pub(super) struct Usage<'app> {
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
pub(super) fn slots_note(app: &App, now: u64) -> String {
    let health = app.live.status.health(now);
    format!("{} running · {} queued", health.running, health.waiting)
}

/// `line` as a sentence, its first letter a capital.
pub(super) fn sentence(line: &str) -> String {
    let mut chars = line.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// `3/9 Run tests`: the step a job is in.
pub(super) fn step_label(job: &Job) -> Option<(String, String)> {
    watch::current_step(job)
        .map(|(step, position)| (format!("{position}/{}", job.steps.len()), step.name.clone()))
}

pub(super) fn slot_lines(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
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

pub(super) fn slot_cards(
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
pub(super) fn card_title(profile: &ProfileStatus, slot: &SlotStatus) -> String {
    format!("{} · slot {}", profile.name, slot.slot)
}

/// A slot card's size in its title, from the fullest to none.
pub(super) fn card_sizes(profile: &ProfileStatus) -> [String; 3] {
    [
        format!(" {} · {} CPUs ", bytes(profile.memory), profile.cpus),
        format!(" {} ", bytes(profile.memory)),
        String::new(),
    ]
}

/// What a slot card shows: its job with step, time, memory and CPU; its runner waiting for its job, or warm for the
/// next one; or nothing.
pub(super) fn card(
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
