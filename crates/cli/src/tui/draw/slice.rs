//! The overview's charts of the slice's memory and CPU over the last half hour.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Sparkline};
use runnerd_core::api::{cpus, in_cores};

use super::Theme;
use super::bold;
use crate::human::bytes;
use crate::tui::app::App;

/// The slice's memory and CPU, every runner container together, as filled charts against their limits.
pub(super) fn slice(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
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
