//! The jobs view's timeline lens: a lane per slot that ran jobs in the range, from the history alone (ADR 0135). A job's
//! bar in its outcome's colour; a stretch without a job as core words it, `no job` or `unknown`, never idle by
//! guess; waits no runner took in a lane of their own; then the bars whose end core words.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{self, Gap, GapKind, SlotTimeline, TimelineBar};
use unicode_width::UnicodeWidthStr;

use super::{Theme, fit, length};
use crate::human;
use crate::tui::app::App;
use crate::tui::reading::{Reading, Unread};

/// A cell of a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    Empty,
    Gap(GapKind),
    Bar(api::OutcomeKind),
    Waiting,
}

impl Mark {
    pub(super) fn glyph(self) -> &'static str {
        match self {
            Mark::Empty => " ",
            Mark::Gap(GapKind::NoJob) => "·",
            Mark::Gap(GapKind::Unknown) => "╱",
            Mark::Bar(_) => "█",
            Mark::Waiting => "░",
        }
    }

    fn style(self, theme: Theme) -> Style {
        match self {
            Mark::Empty | Mark::Gap(_) => theme.dim(),
            Mark::Bar(kind) => theme.tone(human::outcome_tone(kind)),
            Mark::Waiting => theme.wait(),
        }
    }
}

/// The range's timeline, or why there is none.
pub(super) fn timeline(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let timeline = match &app.history.timeline {
        Reading::Read(timeline) => timeline,
        Reading::Unread(Unread::Waiting) => {
            frame.render_widget(Line::styled("reading…", theme.dim()), area);
            return;
        }
        Reading::Unread(Unread::Unsupported) => {
            frame.render_widget(
                Line::styled(format!("timeline {}", api::NOT_SUPPORTED), theme.dim()),
                area,
            );
            return;
        }
        Reading::Unread(Unread::Failed(why)) => {
            frame.render_widget(
                Line::styled(why.clone(), theme.tone(human::Tone::Bad)),
                area,
            );
            return;
        }
    };
    let timeline = &from_first_bar(timeline);
    let label_width = timeline
        .lanes
        .iter()
        .map(|lane| lane_label(&lane.profile, lane.slot).width())
        .chain([UNASSIGNED.width()])
        .max()
        .unwrap_or(0)
        .min(16);
    let cells = usize::from(area.width).saturating_sub(label_width + 1);
    let mut lines = vec![axis(timeline, label_width, cells, now, theme)];
    for lane in &timeline.lanes {
        let marks = lane_marks(timeline, &lane.gaps(timeline), &lane.bars, cells);
        let style = theme.profile(app.profile_index(&lane.profile));
        let mut spans = vec![Span::styled(
            format!(
                "{} ",
                fit(&lane_label(&lane.profile, lane.slot), label_width)
            ),
            style,
        )];
        spans.extend(marks_spans(&marks, theme));
        lines.push(Line::from(spans));
    }
    if !timeline.unassigned.is_empty() {
        let marks = lane_marks(timeline, &[], &timeline.unassigned, cells);
        let mut spans = vec![Span::styled(
            format!("{} ", fit(UNASSIGNED, label_width)),
            theme.wait(),
        )];
        spans.extend(marks_spans(&marks, theme));
        lines.push(Line::from(spans));
    }
    lines.push(legend(theme));
    // The bars whose end core words, each with its job.
    let ends: Vec<&TimelineBar> = timeline
        .lanes
        .iter()
        .flat_map(|lane| &lane.bars)
        .chain(&timeline.unassigned)
        .filter(|bar| bar.end_words().is_some())
        .collect();
    let [chart, rest] =
        Layout::vertical([Constraint::Length(length(lines.len())), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(lines), chart);
    let width = usize::from(rest.width);
    let notes: Vec<Line<'_>> = ends
        .iter()
        .map(|bar| {
            let words = bar.end_words().unwrap_or_default();
            Line::from(vec![
                Span::raw(fit(
                    &bar.title().to_string(),
                    width.saturating_sub(words.width() + 2),
                )),
                Span::styled(format!("  {words}"), theme.dim()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(notes), rest);
}

/// `timeline` from its first bar on, so a range longer than the jobs in it does not squeeze them into its end. The
/// gaps core words are cut to it with the window.
fn from_first_bar(timeline: &SlotTimeline) -> SlotTimeline {
    let first = timeline
        .lanes
        .iter()
        .flat_map(|lane| &lane.bars)
        .chain(&timeline.unassigned)
        .map(|bar| bar.queued_at)
        .min()
        .map_or(timeline.since, |first| {
            first.clamp(timeline.since, timeline.until)
        });
    let mut shown = timeline.clone();
    shown.since = first;
    shown
}

/// The lane of waits no runner took.
pub(super) const UNASSIGNED: &str = SlotTimeline::UNASSIGNED;

/// A lane's label: `jvm 1`.
pub(super) fn lane_label(profile: &str, slot: usize) -> String {
    format!("{profile} {slot}")
}

/// The window's start and end over the lanes, as ages.
fn axis(
    timeline: &SlotTimeline,
    label_width: usize,
    cells: usize,
    now: u64,
    theme: Theme,
) -> Line<'static> {
    let from = api::ago(timeline.since, now);
    let to = api::ago(timeline.until, now);
    let pad = cells.saturating_sub(from.width() + to.width());
    Line::styled(
        format!("{:label_width$} {from}{}{to}", "", " ".repeat(pad)),
        theme.dim(),
    )
}

/// What each glyph is, in core's words for the gaps.
fn legend(theme: Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (mark, words) in [
        (Mark::Bar(api::OutcomeKind::Succeeded), "job"),
        (Mark::Waiting, "waiting"),
        (Mark::Gap(GapKind::NoJob), GapKind::NoJob.words()),
        (Mark::Gap(GapKind::Unknown), GapKind::Unknown.words()),
    ] {
        spans.push(Span::styled(mark.glyph(), mark.style(theme)));
        spans.push(Span::styled(format!(" {words}   "), theme.dim()));
    }
    Line::from(spans)
}

fn marks_spans(marks: &[Mark], theme: Theme) -> Vec<Span<'static>> {
    marks
        .iter()
        .map(|mark| Span::styled(mark.glyph(), mark.style(theme)))
        .collect()
}

/// The `cells` of a lane over `timeline`'s window: each cell the last of its gaps, then its bars, that overlaps it. A
/// bar's wait before its start is not the slot's, so only a bar that never started is drawn waiting.
pub(super) fn lane_marks(
    timeline: &SlotTimeline,
    gaps: &[Gap],
    bars: &[TimelineBar],
    cells: usize,
) -> Vec<Mark> {
    let mut marks = vec![Mark::Empty; cells];
    let span = timeline.until.saturating_sub(timeline.since).max(1);
    let cell_of = |at: u64| -> usize {
        let offset = at.saturating_sub(timeline.since).min(span);
        usize::try_from(u128::from(offset) * cells as u128 / u128::from(span)).unwrap_or(cells)
    };
    let mut fill = |from: u64, to: u64, mark: Mark| {
        if to < timeline.since || from > timeline.until || cells == 0 {
            return;
        }
        let first = cell_of(from).min(cells - 1);
        let last = cell_of(to.saturating_sub(1).max(from)).min(cells - 1);
        for cell in &mut marks[first..=last] {
            *cell = mark;
        }
    };
    for gap in gaps {
        fill(gap.span.from, gap.span.to, Mark::Gap(gap.kind));
    }
    for bar in bars {
        let end = bar.end.at().unwrap_or(timeline.until);
        match bar.started_at {
            Some(started) => fill(started, end, Mark::Bar(bar.outcome().kind)),
            None => fill(bar.queued_at, end, Mark::Waiting),
        }
    }
    marks
}
