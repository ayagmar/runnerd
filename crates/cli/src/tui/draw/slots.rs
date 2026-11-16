//! The overview's slots, a lane of one row each in the budget strip's order: a running slot's job, step, time,
//! memory and CPU; an idle or warm slot its state, quiet.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{Job, ProfileStatus, Sample, SlotStatus, Status};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::{Window, bold, fit, picked, spark};
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

/// A lane per slot, in config order as the budget strip draws them, as many as `area` has rows. A running slot: its
/// square in the profile's colour, its name, then its job's columns, each as wide as the widest lane needs it (see
/// [`Columns`]). An idle or warm slot: its state, dim, and when its runner started where that fits.
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
    // The selected slot stays in sight; short of a row per slot, the last row says how many more there are.
    let window = Window::of(
        slots.len(),
        usize::from(area.height),
        Some(if focused { app.slot } else { 0 }),
    );
    let (first, shown) = (window.first, window.shown);
    // A long profile name takes a third of the row at most, cut before its slot's number, which always shows.
    let name_width = slots
        .iter()
        .map(|(_, profile, slot)| format!("{} {}", profile.name, slot.slot).width())
        .max()
        .unwrap_or(0)
        .min((width / 3).max(10));
    let rest = width.saturating_sub(2 + name_width + 1);
    let running: Vec<Option<Lane<'_>>> = slots
        .iter()
        .map(|(_, profile, slot)| {
            let state = Status::slot_state(profile, slot);
            slot_job(app, profile, slot)
                .filter(|_| state.is_running())
                .map(|job| Lane::of(app, profile, job, now))
        })
        .collect();
    let columns = Columns::of(&running.iter().flatten().collect::<Vec<_>>(), rest);
    let lines: Vec<Line<'_>> = slots
        .iter()
        .zip(&running)
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(position, ((index, profile, slot), lane))| {
            let mut spans = vec![
                Span::styled("■ ", theme.profile(Some(*index))),
                Span::styled(
                    format!("{} ", slot_name(profile, slot.slot, name_width)),
                    bold(),
                ),
            ];
            if let Some(lane) = lane {
                spans.extend(columns.spans(lane, theme));
            } else {
                let state = Status::slot_state(profile, slot);
                // Its sentence whole, or its word.
                let state = Some(state.line())
                    .filter(|line| line.width() <= rest)
                    .unwrap_or_else(|| state.word().to_owned());
                let started = slot
                    .runner
                    .as_ref()
                    .map(|runner| format!(" · started {}", human::ago(runner.started_at, now)))
                    .filter(|started| state.width() + started.width() <= rest)
                    .unwrap_or_default();
                spans.push(Span::styled(
                    fit(&format!("{state}{started}"), rest)
                        .trim_end()
                        .to_owned(),
                    theme.dim(),
                ));
            }
            let line = Line::from(spans);
            if focused && position == app.slot {
                picked(line)
            } else {
                line
            }
        })
        .collect();
    let mut lines = lines;
    lines.extend(window.more_line(theme));
    hits.rows(area, List::Slots, first, shown);
    frame.render_widget(Paragraph::new(lines), area);
}

/// A slot's name in `width` cells, padded: its profile's name, cut where it must, and its number whole.
fn slot_name(profile: &ProfileStatus, slot: usize, width: usize) -> String {
    let number = format!(" {slot}");
    let name = format!("{}{number}", profile.name);
    if name.width() <= width {
        return fit(&name, width);
    }
    let cut = fit(&profile.name, width.saturating_sub(number.width()));
    format!("{}{number}", cut.trim_end())
}

/// What a running slot's lane shows, as text, with its samples.
struct Lane<'app> {
    title: String,
    repository: String,
    count: String,
    step: String,
    ran: String,
    memory: String,
    cpu: String,
    usage: Usage<'app>,
}

impl Lane<'_> {
    fn of<'app>(app: &'app App, profile: &ProfileStatus, job: &Job, now: u64) -> Lane<'app> {
        let usage = Usage::of(app, profile, job);
        let (count, step) = step_label(job).unwrap_or_default();
        Lane {
            title: job.summary.title().to_string(),
            repository: job.summary.repository.clone(),
            count,
            step,
            ran: job
                .summary
                .timeline
                .started_at
                .map_or_else(String::new, |at| duration(now.saturating_sub(at))),
            memory: usage.last().map_or_else(String::new, |s| bytes(s.memory)),
            cpu: usage.last().map_or_else(String::new, |s| cores_used(s.cpu)),
            usage,
        }
    }
}

/// The running lanes' columns, in cells, each as wide as its widest text; 0 for a column left out. Short of room the
/// sparklines go first, then the repository, then the step's name; the job's name is cut last. Room past them all
/// widens the sparklines, up to [`SPARK_MOST`] cells each, and then lies between the names and the figures.
struct Columns {
    title: usize,
    repository: usize,
    count: usize,
    step: usize,
    ran: usize,
    memory: usize,
    cpu: usize,
    spark: usize,
    /// The cells between the repository and the step.
    slack: usize,
}

/// The fewest cells a step's name is cut to rather than left out.
const STEP_LEAST: usize = 12;

/// The fewest cells a sparkline is drawn in.
const SPARK_LEAST: usize = 6;

/// The most cells a sparkline takes: a lane's samples cover about a minute.
const SPARK_MOST: usize = 30;

impl Columns {
    fn of(lanes: &[&Lane<'_>], width: usize) -> Columns {
        let widest = |text: &dyn Fn(&Lane<'_>) -> usize| {
            lanes.iter().map(|lane| text(lane)).max().unwrap_or(0)
        };
        let mut columns = Columns {
            title: widest(&|lane| lane.title.width()),
            repository: widest(&|lane| lane.repository.width()),
            count: widest(&|lane| lane.count.width()),
            step: widest(&|lane| lane.step.width()),
            ran: widest(&|lane| lane.ran.width()),
            memory: widest(&|lane| lane.memory.width()),
            cpu: widest(&|lane| lane.cpu.width()),
            spark: SPARK_LEAST,
            slack: 0,
        };
        // The cells each column takes with the one before it.
        let used = |columns: &Columns| {
            let gap = |width: usize| if width > 0 { width + 1 } else { 0 };
            columns.title
                + gap(columns.repository)
                + gap(columns.count)
                + gap(columns.step)
                + gap(columns.ran)
                + gap(columns.memory)
                + gap(columns.spark)
                + gap(columns.cpu)
                + gap(columns.spark)
        };
        if used(&columns) > width {
            columns.spark = 0;
        }
        if used(&columns) > width {
            columns.repository = 0;
        }
        let step = columns.step;
        if used(&columns) > width {
            columns.step = 0;
        }
        let over = used(&columns).saturating_sub(width);
        columns.title = columns.title.saturating_sub(over);
        if columns.spark > 0 {
            let spare = width.saturating_sub(used(&columns)) / 2;
            columns.spark = (SPARK_LEAST + spare).min(SPARK_MOST);
        }
        // A step's name dropped whole comes back cut where a dozen cells are left for it.
        let left = width.saturating_sub(used(&columns) + 1);
        if columns.step == 0 && step > 0 && left >= STEP_LEAST {
            columns.step = step.min(left);
        }
        columns.slack = width.saturating_sub(used(&columns));
        columns
    }

    fn spans(&self, lane: &Lane<'_>, theme: Theme) -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw(fit(&lane.title, self.title))];
        if self.repository > 0 {
            spans.push(Span::styled(
                format!(" {}", fit(&lane.repository, self.repository)),
                theme.dim(),
            ));
        }
        spans.push(Span::raw(" ".repeat(self.slack)));
        if self.count > 0 {
            spans.push(Span::styled(
                format!(" {:>width$}", lane.count, width = self.count),
                theme.cpu(),
            ));
        }
        if self.step > 0 {
            spans.push(Span::raw(format!(" {}", fit(&lane.step, self.step))));
        }
        if self.ran > 0 {
            spans.push(Span::raw(format!(
                " {:>width$}",
                lane.ran,
                width = self.ran
            )));
        }
        if self.memory > 0 {
            spans.push(Span::raw(format!(
                " {:>width$}",
                lane.memory,
                width = self.memory
            )));
        }
        if self.spark > 0 {
            spans.push(Span::styled(
                format!(
                    " {:<width$}",
                    lane.usage.memory(self.spark),
                    width = self.spark
                ),
                theme.memory(),
            ));
        }
        if self.cpu > 0 {
            spans.push(Span::raw(format!(
                " {:>width$}",
                lane.cpu,
                width = self.cpu
            )));
        }
        if self.spark > 0 {
            spans.push(Span::styled(
                format!(" {}", lane.usage.cpu(self.spark)),
                theme.cpu(),
            ));
        }
        spans
    }
}
