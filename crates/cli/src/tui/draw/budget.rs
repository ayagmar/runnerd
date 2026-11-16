//! The overview's budget strip: a segment per slot as wide as its share of the slice's memory, filled as far as its
//! job uses it now, then the mirror, each seed container and shell, what is free and what is closed; each segment
//! labelled under itself with its name and its figure, in its colour. What no segment has room to name goes in the
//! heading; a slot's lane under the strip names it anyway.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use runnerd_core::api::{BudgetParts, HeldKind};
use runnerd_core::config::ByteSize;
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::heading;
use super::job::fit_details;
use super::slots::slot_job;
use crate::human::bytes;
use crate::tui::app::App;

/// `part` of `whole` as cells of `width`.
pub(super) fn scale(part: u64, whole: u64, width: u16) -> u16 {
    let cells = u128::from(part) * u128::from(width) / u128::from(whole.max(1));
    u16::try_from(cells).unwrap_or(width).min(width)
}

/// The rows the strip takes: its heading, `bar` rows of segments and two of labels.
pub(super) fn height(bar: u16) -> u16 {
    1 + bar + 2
}

/// The budget's heading: what is reserved of what, how runners are admitted, and what the slice uses, each with its
/// rank for [`fit_details`].
pub(super) fn budget_note(app: &App) -> Vec<(u8, String)> {
    let budget = &app.live.status.budget;
    let mut parts = vec![(0, budget.memory_reading().line())];
    if let Some(cpu) = budget.cpu {
        parts.push((1, cpu.line()));
    }
    if let Some(host) = budget.host_line() {
        parts.push((1, host));
    }
    if let Some(host) = &app.live.host {
        parts.push((2, format!("{} in use, sampled", bytes(host.memory))));
    }
    parts
}

/// One segment of the strip.
struct Segment {
    /// Its cells, the gap after it included.
    cells: u16,
    /// Of those, the cells its job uses now.
    used: u16,
    /// The glyph of the rest: reserved, idle, held, free or closed.
    glyph: char,
    style: Style,
    /// Its name under it, longest first, such as `jvm 1`, `mirror` or `held by the host`, `host`.
    names: Vec<String>,
    /// Its figure under the name, longest first: `3.9G of 9G`, `3.9G`.
    figures: Vec<String>,
    /// Whether the heading names it when it has no room: what a slot's lane does not show.
    named_above: bool,
    /// Whether it ends in a gap before the next segment.
    gap: bool,
}

impl Segment {
    fn room(&self) -> usize {
        usize::from(self.cells.saturating_sub(u16::from(self.gap)))
    }
}

/// The strip in `area`: its heading, its segments in all rows but the last two, and the labels under them.
pub(super) fn budget(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let status = &app.live.status;
    let [title, bar, names_area, figures_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let parts = status.budget_parts();
    let segments = segments(app, &parts, bar.width, theme);
    let mut note = budget_note(app);

    let mut cells = Vec::new();
    for segment in segments.iter().filter(|segment| segment.cells > 0) {
        let rest = usize::from(
            segment
                .cells
                .saturating_sub(segment.used + u16::from(segment.gap)),
        );
        cells.push(Span::styled(
            "█".repeat(usize::from(segment.used)),
            segment.style,
        ));
        cells.push(Span::styled(
            segment.glyph.to_string().repeat(rest),
            segment.style,
        ));
        if segment.gap {
            cells.push(Span::raw(" "));
        }
    }
    let bar_line = Line::from(cells);
    for row in 0..bar.height {
        frame.render_widget(
            bar_line.clone(),
            Rect {
                y: bar.y + row,
                height: 1,
                ..bar
            },
        );
    }

    let placed = place(&segments);
    for (segment, label) in segments.iter().zip(&placed) {
        if segment.named_above && label.is_none() {
            let name = segment.names.first().cloned().unwrap_or_default();
            let figure = segment.figures.last().cloned().unwrap_or_default();
            note.push((3, format!("{name} {figure}")));
        }
    }
    let names: Vec<Option<String>> = placed
        .iter()
        .map(|label| label.as_ref().map(|label| label.name.clone()))
        .collect();
    let figures: Vec<Option<String>> = placed
        .iter()
        .map(|label| label.as_ref().map(|label| label.figure.clone()))
        .collect();
    frame.render_widget(labels(&segments, &names, Modifier::BOLD, theme), names_area);
    frame.render_widget(
        labels(&segments, &figures, Modifier::empty(), theme),
        figures_area,
    );
    let room = usize::from(title.width).saturating_sub("Budget".width() + 2);
    heading(frame, title, "Budget", fit_details(note, room), theme);
}

/// A row of labels, each at its segment's first cell: in the terminal's own foreground, which reads on a dark and a
/// light one alike, the bar above carrying the profile's colour; a dim segment's label dim.
fn labels(
    segments: &[Segment],
    placed: &[Option<String>],
    modifier: Modifier,
    theme: Theme,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut x = 0;
    let mut at = 0;
    for (segment, label) in segments.iter().zip(placed) {
        if let Some(label) = label {
            spans.push(Span::raw(" ".repeat(at - x)));
            let ink = if segment.style == theme.dim() {
                theme.dim()
            } else {
                Style::new()
            };
            spans.push(Span::styled(label.clone(), ink.add_modifier(modifier)));
            x = at + label.width();
        }
        at += usize::from(segment.cells);
    }
    Line::from(spans)
}

/// A segment's name and figure, one above the other.
struct Label {
    name: String,
    figure: String,
}

impl Label {
    fn width(&self) -> usize {
        self.name.width().max(self.figure.width())
    }
}

/// A segment's labels, widest first: each name with each figure, then a slot's name alone.
fn options(segment: &Segment) -> Vec<Label> {
    let mut options: Vec<Label> = segment
        .names
        .iter()
        .flat_map(|name| {
            segment.figures.iter().map(|figure| Label {
                name: name.clone(),
                figure: figure.clone(),
            })
        })
        .collect();
    if !segment.named_above {
        options.extend(segment.names.iter().map(|name| Label {
            name: name.clone(),
            figure: String::new(),
        }));
    }
    options.sort_by_key(|label| std::cmp::Reverse(label.width()));
    options
}

/// Which of each segment's labels goes under it, left to right: the widest that fits from its first cell to the next
/// segment that has room for a label of its own, past a cell after the label before it. A label is never cut, and a
/// figure is never shown without its name.
fn place(segments: &[Segment]) -> Vec<Option<Label>> {
    let options: Vec<Vec<Label>> = segments.iter().map(options).collect();
    let fits_own = |at: usize| {
        options[at]
            .iter()
            .any(|label| label.width() <= segments[at].room())
    };
    let starts: Vec<usize> = segments
        .iter()
        .scan(0, |x, segment| {
            let start = *x;
            *x += usize::from(segment.cells);
            Some(start)
        })
        .collect();
    let mut cursor = 0;
    let mut placed = Vec::new();
    for at in 0..segments.len() {
        let start = starts[at];
        if start < cursor || segments[at].cells == 0 {
            placed.push(None);
            continue;
        }
        let mut end = start + segments[at].room();
        for next in at + 1..segments.len() {
            if fits_own(next) {
                break;
            }
            end = starts[next] + segments[next].room();
        }
        let label = options[at]
            .iter()
            .find(|label| label.width() <= end - start)
            .map(|label| Label {
                name: label.name.clone(),
                figure: label.figure.clone(),
            });
        if let Some(label) = &label {
            cursor = start + label.width() + 1;
        }
        placed.push(label);
    }
    placed
}

/// What fills a slot's segment: the cells its job uses, the glyph of the rest, and its figures, longest first.
struct Fill {
    used: u16,
    glyph: char,
    figures: Vec<String>,
}

impl Fill {
    /// A slot of `memory`, with a runner or not, its job's sampled memory if there is one, `room` cells wide.
    fn of(runner: bool, sample: Option<u64>, memory: u64, room: u16) -> Fill {
        match (runner, sample) {
            (true, Some(sample)) => Fill {
                used: scale(sample, memory, room),
                glyph: '▓',
                figures: vec![
                    format!("{} of {}", bytes(sample), bytes(memory)),
                    format!(
                        "{} of {}",
                        ByteSize(sample).short(),
                        ByteSize(memory).short()
                    ),
                    bytes(sample),
                    ByteSize(sample).short(),
                ],
            },
            (true, None) => Fill {
                used: 0,
                glyph: '▓',
                figures: vec![bytes(memory), ByteSize(memory).short()],
            },
            (false, _) => Fill {
                used: 0,
                glyph: '░',
                figures: vec![format!("idle {}", bytes(memory)), "idle".to_owned()],
            },
        }
    }
}

/// The strip's segments for `width` cells: the slots in config order, what holds memory for something else, what is
/// free and what is closed. The closed part takes what rounding leaves, so the strip never shows more free than core
/// says is.
fn segments(app: &App, parts: &BudgetParts, width: u16, theme: Theme) -> Vec<Segment> {
    let status = &app.live.status;
    let whole = parts.whole;
    let mut segments = Vec::new();
    let mut drawn: u16 = 0;
    for part in &parts.slots {
        let Some(profile) = status.profiles.get(part.profile) else {
            continue;
        };
        let Some(slot) = profile.slots.iter().find(|slot| slot.slot == part.slot) else {
            continue;
        };
        let cells = scale(part.memory, whole, width)
            .max(1)
            .min(width.saturating_sub(drawn));
        if cells == 0 {
            break;
        }
        let sample =
            slot_job(app, profile, slot).and_then(|job| app.live.samples.get(&job.summary.id));
        let fill = Fill::of(
            slot.runner.is_some(),
            sample.map(|sample| sample.memory),
            part.memory,
            cells.saturating_sub(1),
        );
        segments.push(Segment {
            cells,
            used: fill.used,
            glyph: fill.glyph,
            style: theme.profile(Some(part.profile)),
            names: vec![format!("{} {}", profile.name, slot.slot)],
            figures: fill.figures,
            named_above: false,
            gap: true,
        });
        drawn += cells;
    }
    for held in &parts.held {
        let style = match &held.kind {
            HeldKind::Seed { profile } => theme.profile(
                status
                    .profiles
                    .iter()
                    .position(|status| &status.name == profile),
            ),
            HeldKind::Mirror | HeldKind::Shell { .. } => Style::default(),
        };
        let cells = scale(held.memory, whole, width)
            .max(2)
            .min(width.saturating_sub(drawn));
        segments.push(Segment {
            cells,
            used: 0,
            glyph: '▒',
            style,
            names: vec![held.name()],
            figures: vec![bytes(held.memory), ByteSize(held.memory).short()],
            named_above: true,
            gap: true,
        });
        drawn += cells;
    }
    let rest = width.saturating_sub(drawn);
    let free_cells = match (&parts.free, &parts.closed) {
        (Some(free), Some(_)) => scale(free.memory, whole, width).min(rest.saturating_sub(1)),
        (Some(_), None) => rest,
        (None, _) => 0,
    };
    if let Some(free) = &parts.free {
        segments.push(Segment {
            cells: free_cells,
            used: 0,
            glyph: '·',
            style: theme.dim(),
            names: vec!["free".to_owned()],
            figures: vec![free.reading.figure.clone()],
            named_above: true,
            gap: false,
        });
    }
    if let Some(closed) = &parts.closed {
        segments.push(Segment {
            cells: rest - free_cells,
            used: 0,
            glyph: '╱',
            style: theme.dim(),
            names: vec![
                closed.kind.name().to_owned(),
                closed.kind.short().to_owned(),
            ],
            figures: vec![closed.reading.figure.clone()],
            named_above: true,
            gap: false,
        });
    }
    segments
}
