//! One job's steps on its time axis.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{self, Job, Step};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::budget::scale;
use super::{bold, fit, heading};
use crate::human::{Tone, duration};
use crate::watch;

/// Each step as a bar on the job's time axis, with its duration. Steps that do not fit are left out around the
/// one in progress.
pub(super) fn steps(frame: &mut Frame<'_>, area: Rect, job: &Job, now: u64, theme: Theme) {
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

pub(super) fn step_line(
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
