//! One job, in the order the dashboard's page has: its header, timeline, steps, memory and CPU, the latest runs of the
//! same job, its log.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{
    Job, JobSummary, Phase, PhaseKind, RecordNote, RecordState, RunComparison,
};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::budget::scale;
use super::log::log;
use super::overview::{badge, reason};
use super::resources::{job_charts, job_spark};
use super::steps::steps;
use super::{bold, fit, heading, length, spark};
use crate::human::{self, bytes, duration};
use crate::tui::app::{App, JobView};

pub(super) fn job(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    open: &JobView,
    now: u64,
    theme: Theme,
) {
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
    // What held its CPU back takes a row of its own under the figures.
    let cpu_held = summary.cpu_held(now).filter(|_| charted);
    let held_height = u16::from(cpu_held.is_some());
    let full_charts = charted && area.height >= fixed + steps_wanted + 7 + held_height + 6;
    let charts_height = held_height
        + match (charted, full_charts) {
            (false, _) => 0,
            (true, true) => 7,
            (true, false) => 2,
        };
    let steps_height = steps_wanted.min(area.height.saturating_sub(fixed + charts_height + 4));
    let [
        head,
        timeline_area,
        steps_area,
        charts_area,
        runs_area,
        log_area,
    ] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Length(steps_height),
        Constraint::Length(charts_height),
        Constraint::Length(runs_height),
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
    steps(frame, steps_area, job, now, theme);
    let cpu_held = cpu_held.as_deref();
    match (charted, full_charts) {
        (false, _) => {}
        (true, true) => job_charts(frame, charts_area, app, open, cpu_held, theme),
        (true, false) => job_spark(frame, charts_area, app, open, cpu_held, theme),
    }
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
    log(frame, log_area, open, theme);
}

/// The open job's details under its name, each with its rank: when they do not fit one line, the least important
/// go first.
pub(super) fn job_details(app: &App, job: &Job, now: u64) -> Vec<(u8, String)> {
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
        let waited = match summary.unseen_note(now) {
            Some(note) => format!("waited {}, {note}", duration(waited)),
            None => format!("waited {}", duration(waited)),
        };
        details.push((2, waited));
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
pub(super) fn fit_details(mut details: Vec<(u8, String)>, width: usize) -> String {
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
pub(super) fn drop_least<T>(items: &mut Vec<(u8, T)>, kept: u8, fit: impl Fn(&[(u8, T)]) -> bool) {
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
pub(super) fn timeline(
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
pub(super) fn job_runs(
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
