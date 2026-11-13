//! The open job beside its baseline, in place of its log: what it is compared against, or why nothing, then each
//! whole-job figure on both sides with its change in core's tone, then each step's time. `c` and Esc go back to the log.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use runnerd_core::api::{self, FigureDelta, JobComparison};
use unicode_width::UnicodeWidthStr;

use super::{Theme, bold, fit, wrap};
use crate::tui::app::JobView;
use crate::tui::reading::{Reading, Unread};

/// The width of a figure's value column.
const VALUE: usize = 12;

pub(super) fn compare(frame: &mut Frame<'_>, area: Rect, open: &JobView, now: u64, theme: Theme) {
    let comparison = match &open.comparison {
        Reading::Read(comparison) => Some(comparison),
        Reading::Unread(_) => None,
    };
    let note = comparison
        .and_then(|comparison| comparison.baseline.against(now))
        .unwrap_or_default();
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(theme.dim())
        .title(Span::styled(" Compared with an earlier run ", bold()))
        .title_top(Line::styled(format!(" {note} "), theme.dim()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width);
    let lines = match &open.comparison {
        Reading::Read(comparison) => lines(comparison, width, theme),
        Reading::Unread(Unread::Waiting) => vec![Line::styled("reading…", theme.dim())],
        Reading::Unread(Unread::Unsupported) => {
            vec![Line::styled(
                api::Feature::Comparison.unsupported(),
                theme.dim(),
            )]
        }
        Reading::Unread(Unread::Failed(why)) => wrap(why, width)
            .into_iter()
            .map(|line| Line::styled(line, theme.of(api::Tone::Bad)))
            .collect(),
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn lines(comparison: &JobComparison, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(why) = comparison.baseline.words() {
        lines.extend(
            wrap(&why, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.dim())),
        );
    }
    let label_width = comparison
        .figures
        .iter()
        .map(|figure| figure.figure.label().width())
        .chain(comparison.steps.iter().map(|step| step.name.width()))
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(3 * VALUE + 6) / 2)
        .max(8);
    if !comparison.figures.is_empty() {
        lines.push(Line::styled(
            format!(
                "{:label_width$}  {:>VALUE$}  {:>VALUE$}  {}",
                "", "this run", "baseline", "change"
            ),
            theme.dim(),
        ));
    }
    for figure in &comparison.figures {
        lines.push(row(figure.figure.label(), figure, None, label_width, theme));
    }
    if !comparison.steps.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled("Steps", bold()));
    }
    for step in &comparison.steps {
        lines.push(row(
            &step.name,
            &step.time,
            step.matched.words(),
            label_width,
            theme,
        ));
    }
    if let Some(cut) = comparison.steps_cut_words() {
        lines.extend(
            wrap(cut, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.dim())),
        );
    }
    lines
}

/// A figure's row: its label, this run's and the baseline's value, and the change in its tone; a step not paired
/// says why instead of a change.
fn row(
    label: &str,
    figure: &FigureDelta,
    unpaired: Option<&str>,
    label_width: usize,
    theme: Theme,
) -> Line<'static> {
    let change = if let Some(words) = unpaired {
        Span::styled(words.to_owned(), theme.dim())
    } else {
        let style = match figure.tone() {
            api::Tone::Neutral => Style::new(),
            tone => theme.of(tone),
        };
        Span::styled(figure.change_words(), style)
    };
    Line::from(vec![
        Span::raw(format!("{}  ", fit(label, label_width))),
        Span::raw(format!("{:>VALUE$}  ", figure.this_words())),
        Span::styled(
            format!("{:>VALUE$}  ", figure.baseline_words()),
            theme.dim(),
        ),
        change,
    ])
}
