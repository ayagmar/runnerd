//! One job's memory and CPU: the charts, their figures, and what held the job back.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use runnerd_core::api::{self, ChartLine, ChartLineKind, ChartLines, Job, in_cores};
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::slice::Chart;
use super::{bold, fit, spark};
use crate::human::{bytes, cores_used};
use crate::tui::app::{App, JobView};

/// The open job's chart lines, as `Job::chart_lines` labels them for every client.
pub(super) fn chart_lines(app: &App, job: &Job) -> ChartLines {
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
pub(super) fn highest(lines: &[ChartLine]) -> u64 {
    lines.iter().map(|line| line.value).max().unwrap_or(0)
}

/// A job's chart of `values` with its lines: the cap on top, the reservation under it.
pub(super) fn job_chart(
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
pub(super) fn metrics_line(open: &JobView, width: usize, theme: Theme) -> Line<'static> {
    let metrics = &open.job.summary.metrics;
    let peak_memory = metrics
        .memory_peak
        .or_else(|| open.samples.iter().map(|sample| sample.memory).max());
    let peak_cpu = metrics
        .cpu_peak
        .or_else(|| open.samples.iter().map(|sample| sample.cpu).max());
    // A job still running has no totals yet: its samples stand in for the peaks.
    let mut totals = api::JobMetrics {
        memory_peak: peak_memory,
        cpu_peak: peak_cpu,
        ..metrics.clone()
    }
    .totals();
    if let Some(activity) = metrics.build_cache.activity() {
        let what = match activity.counts.hit_percent() {
            Some(percent) => format!("{percent}% hit"),
            None => activity
                .refused_line()
                .unwrap_or_else(|| activity.stored_line()),
        };
        totals.push(api::Total {
            label: "Build cache",
            value: what,
        });
    }
    let parts = totals;
    let mut line = Line::default();
    for api::Total { label, value } in parts {
        let gap = if line.spans.is_empty() { "" } else { "  " };
        if line.width() + gap.width() + label.width() + 1 + value.width() > width {
            break;
        }
        line.push_span(Span::styled(format!("{gap}{label} "), theme.dim()));
        line.push_span(Span::styled(value, bold()));
    }
    line
}

pub(super) fn job_charts(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    open: &JobView,
    cpu_held: Option<&str>,
    theme: Theme,
) {
    let [charts, metrics] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(figures_height(cpu_held)),
    ])
    .areas(area);
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
    figures(frame, metrics, open, cpu_held, theme);
}

/// The rows `figures` takes.
pub(super) fn figures_height(cpu_held: Option<&str>) -> u16 {
    1 + u16::from(cpu_held.is_some())
}

/// The job's figures, then what held its CPU back on a row of its own: too long to share the details' line.
pub(super) fn figures(
    frame: &mut Frame<'_>,
    area: Rect,
    open: &JobView,
    cpu_held: Option<&str>,
    theme: Theme,
) {
    let [metrics, held] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    frame.render_widget(
        metrics_line(open, usize::from(metrics.width), theme),
        metrics,
    );
    if let Some(cpu_held) = cpu_held {
        frame.render_widget(
            Line::styled(fit(cpu_held, usize::from(held.width)), theme.dim()),
            held,
        );
    }
}

/// Memory and CPU as one line of sparklines, for a terminal too small for charts.
pub(super) fn job_spark(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    open: &JobView,
    cpu_held: Option<&str>,
    theme: Theme,
) {
    let [sparks, metrics] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(figures_height(cpu_held)),
    ])
    .areas(area);
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
    figures(frame, metrics, open, cpu_held, theme);
}
