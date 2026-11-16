//! The overview's charts of the slice's memory and CPU over the last half hour.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Sparkline};
use runnerd_core::api::{self, cpus, in_cores};

use super::Theme;
use super::bold;
use crate::human::bytes;
use crate::tui::app::App;

/// The rows a chart's limit takes: each row is an eighth of it.
const FULL: u64 = 8;

/// The rows the charts take, their borders included: as many eighths of their limits as the higher of their peaks
/// reaches, at least two; none before the first sample.
pub(super) fn height(app: &App) -> u16 {
    if app.host.is_empty() {
        return 0;
    }
    let budget = &app.live.status.budget;
    let reach = |peak: u64, limit: Option<u64>| match limit.filter(|limit| *limit > 0) {
        Some(limit) => (u128::from(peak) * u128::from(FULL)).div_ceil(u128::from(limit)),
        None => u128::from(FULL),
    };
    let memory = reach(
        app.host
            .iter()
            .map(|sample| sample.memory)
            .max()
            .unwrap_or(0),
        budget.memory_max,
    );
    let cpu = reach(
        app.host.iter().map(|sample| sample.cpu).max().unwrap_or(0),
        budget.slice_cpus,
    );
    let rows = memory.max(cpu).clamp(2, u128::from(FULL));
    u16::try_from(rows).unwrap_or(2) + 2
}

/// The slice's memory and CPU, every runner container together, as filled charts against their limits, each row an
/// eighth of its limit, as many rows as `area` has; short of the rows its peak needs, scaled to the peak.
pub(super) fn slice(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let [memory_area, cpu_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let rows = u64::from(area.height.saturating_sub(2));
    // Short of the rows the peak needs, the chart is scaled to the peak instead.
    let scaled = |limit: u64, peak: u64| (limit * rows / FULL).max(peak);
    let peak = |value: fn(&api::HostSample) -> u64| app.host.iter().map(value).max().unwrap_or(0);
    let status = &app.live.status;
    let memory_max = status.budget.memory_max;
    Chart {
        title: "Memory",
        limit_label: memory_max.map_or_else(String::new, |max| format!("MemoryMax {}", bytes(max))),
        reserved_label: String::new(),
        values: app.host.iter().map(|sample| sample.memory).collect(),
        limit: scaled(memory_max.unwrap_or(0), peak(|sample| sample.memory)),
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
        limit: scaled(
            status.budget.slice_cpus.unwrap_or(0),
            peak(|sample| sample.cpu),
        ),
        unit: in_cores,
        style: theme.cpu(),
    }
    .render(frame, cpu_area, theme);
}

/// A filled chart of the newest values that fit, scaled to `limit` (or to the values' peak when there is none),
/// with the last value in its title and the peak under it.
pub(super) struct Chart {
    pub(super) title: &'static str,
    pub(super) limit_label: String,
    /// Under it on the right, such as a job's reservation.
    pub(super) reserved_label: String,
    pub(super) values: Vec<u64>,
    pub(super) limit: u64,
    pub(super) unit: fn(u64) -> String,
    pub(super) style: Style,
}

impl Chart {
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: Theme) {
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
