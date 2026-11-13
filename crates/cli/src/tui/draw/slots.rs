//! The overview's slots, a lane of one row each: a running slot's job, step, time, memory and CPU; an idle or warm
//! slot its state, quiet.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{Job, ProfileStatus, Sample, SlotStatus, Status};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::{bold, fit, selected, spark};
use crate::human::{self, bytes, cores_used, duration};
use crate::tui::app::{App, Pane};
use crate::tui::mouse::{Hits, List};
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

/// `3/9 Run tests`: the step a job is in.
pub(super) fn step_label(job: &Job) -> Option<(String, String)> {
    watch::current_step(job)
        .map(|(step, position)| (format!("{position}/{}", job.steps.len()), step.name.clone()))
}

/// A lane per slot, in config order, as many as `area` has rows. A running slot: its square in the profile's colour,
/// its name, the job and its repository, the step, how long it has run, memory and CPU now, and from 100 columns their
/// recent samples. An idle or warm slot: its state, dim, and when its runner started.
pub(super) fn lanes(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let focused = app.pane == Pane::Slots;
    let width = usize::from(area.width);
    let slots: Vec<_> = app.slots().collect();
    let shown = usize::from(area.height).min(slots.len());
    // The selected slot stays in sight.
    let first = if focused {
        app.slot.saturating_sub(shown.saturating_sub(1))
    } else {
        0
    };
    let name_width = slots
        .iter()
        .map(|(_, profile, slot)| format!("{} {}", profile.name, slot.slot).width())
        .max()
        .unwrap_or(0)
        .min(14);
    let lines: Vec<Line<'_>> = slots
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(position, (index, profile, slot))| {
            let mut spans = vec![
                Span::styled("■ ", theme.profile(Some(*index))),
                Span::styled(
                    format!(
                        "{} ",
                        fit(&format!("{} {}", profile.name, slot.slot), name_width)
                    ),
                    bold(),
                ),
            ];
            let state = Status::slot_state(profile, slot);
            let rest = width.saturating_sub(2 + name_width + 1);
            if let Some(job) = slot_job(app, profile, slot).filter(|_| state.is_running()) {
                spans.extend(running(app, profile, job, rest, now, theme));
            } else {
                let started = slot
                    .runner
                    .as_ref()
                    .map(|runner| format!(" · started {}", human::ago(runner.started_at, now)))
                    .unwrap_or_default();
                spans.push(Span::styled(
                    fit(&format!("{}{started}", state.line()), rest)
                        .trim_end()
                        .to_owned(),
                    theme.dim(),
                ));
            }
            let line = Line::from(spans);
            if focused && position == app.slot {
                line.style(selected())
            } else {
                line
            }
        })
        .collect();
    hits.rows(area, List::Slots, first, shown);
    frame.render_widget(Paragraph::new(lines), area);
}

/// A running slot's lane past its name, in `width` cells: fixed columns on the right, their sparklines only from 100
/// columns; the job's name takes what is left, then its repository if that fits too.
fn running(
    app: &App,
    profile: &ProfileStatus,
    job: &Job,
    width: usize,
    now: u64,
    theme: Theme,
) -> Vec<Span<'static>> {
    let usage = Usage::of(app, profile, job);
    let (count, name) = step_label(job).unwrap_or_default();
    let ran = job
        .summary
        .timeline
        .started_at
        .map_or_else(String::new, |at| duration(now.saturating_sub(at)));
    let memory = usage.last().map_or_else(String::new, |s| bytes(s.memory));
    let cpu = usage.last().map_or_else(String::new, |s| cores_used(s.cpu));
    let sparks = width >= 90;
    let step_width = if width >= 70 { 16 } else { 0 };
    let fixed = 7 + step_width + 9 + 7 + 6 + if sparks { 12 } else { 0 };
    let job_width = width.saturating_sub(fixed).max(10);
    let title = job.summary.title().to_string();
    let repository = &job.summary.repository;
    let (title, repository) = if title.width() + 2 + repository.width().min(12) <= job_width {
        let pad = job_width - title.width() - 2;
        (format!("{title}  "), fit(repository, pad))
    } else {
        (fit(&title, job_width), String::new())
    };
    let mut spans = vec![
        Span::raw(title),
        Span::raw(repository),
        Span::styled(format!(" {count:>5} "), theme.cpu()),
    ];
    if step_width > 0 {
        spans.push(Span::raw(fit(&name, step_width)));
    }
    spans.extend([
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
    spans
}
