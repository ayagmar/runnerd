//! One job's log as rows: each step a header that folds its lines, each group too, the lines in core's styles with
//! their numbers, and the search's matches marked. Following the log, or on the row the keys are on.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use runnerd_core::api::{self, Job, LineKind, LogOutline, StyledLine};
use unicode_width::UnicodeWidthStr;

use super::{Theme, bold, fit, selected};
use crate::human::{Tone, duration};
use crate::tui::app::JobView;
use crate::tui::log::{Row, Section, SectionKind};
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

pub(super) fn log(
    frame: &mut Frame<'_>,
    area: Rect,
    open: &JobView,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let view = &open.log;
    let rows = view.rows();
    let sections = view.sections();
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(theme.dim())
        .title(Span::styled(" Log ", bold()))
        .title_top(Line::styled(note(open, rows.len()), theme.dim()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if rows.is_empty() {
        let text = match view.outline() {
            Reading::Read(outline) => outline.evidence.words().unwrap_or("no lines").to_owned(),
            Reading::Unread(Unread::Failed(why)) => why.clone(),
            Reading::Unread(Unread::Waiting | Unread::Unsupported) => "reading…".to_owned(),
        };
        frame.render_widget(Line::styled(text, theme.dim()), inner);
        return;
    }
    let height = usize::from(inner.height);
    let last = rows.len().saturating_sub(height);
    let first = view
        .cursor()
        .map_or(last, |cursor| cursor.saturating_sub(height / 2).min(last));
    let gutter = view.read().max(1).to_string().len().max(
        rows.iter()
            .filter_map(|row| match row {
                Row::Line(line) => Some((line + 1).to_string().len()),
                _ => None,
            })
            .max()
            .unwrap_or(1),
    );
    let width = usize::from(inner.width);
    let outline = view.outline().get();
    let lines: Vec<Line<'_>> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(at, row)| {
            let line = match *row {
                Row::Section(index) => section_line(
                    &open.job,
                    sections.get(index).copied(),
                    view.is_open(*row),
                    width,
                    now,
                    theme,
                ),
                Row::Group(index) => {
                    group_line(outline, index, view.is_open(*row), gutter, width, theme)
                }
                Row::Line(number) => match view.line(number) {
                    Some(styled) => styled_line(styled, gutter, width, theme),
                    None => Line::default(),
                },
                Row::Missing(range) => Line::styled(
                    format!(
                        "{:>gutter$} lines {}–{} not read yet",
                        "…",
                        range.from + 1,
                        range.to
                    ),
                    theme.dim(),
                ),
            };
            if view.cursor() == Some(at) {
                line.patch_style(selected())
            } else {
                line
            }
        })
        .collect();
    hits.rows(inner, List::Log, first, rows.len() - first);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The log's heading note: what core says of its evidence, the search and where it stands, and whether it follows.
fn note(open: &JobView, rows: usize) -> String {
    let view = &open.log;
    let mut parts = Vec::new();
    match view.outline() {
        Reading::Read(outline) => parts.extend(outline.evidence.words().map(str::to_owned)),
        Reading::Unread(Unread::Unsupported) => {
            parts.push(format!("outline {}", api::NOT_SUPPORTED));
        }
        Reading::Unread(_) => {}
    }
    if let Some(found) = view.found() {
        let more = if found.has_more() { "+" } else { "" };
        parts.push(format!(
            "/{} {}/{}{more}",
            found.query(),
            found.shown(),
            found.count()
        ));
    }
    parts.push(match view.cursor() {
        None => "following".to_owned(),
        Some(at) => format!("{} rows up, G follows", rows.saturating_sub(at + 1)),
    });
    format!(" {} ", parts.join(" · "))
}

/// A step's header: open or closed, its number and name, how it ended and how long it took, and its lines.
fn section_line(
    job: &Job,
    section: Option<Section>,
    open: bool,
    width: usize,
    now: u64,
    theme: Theme,
) -> Line<'static> {
    let Some(section) = section else {
        return Line::default();
    };
    let fold = if open { "▾ " } else { "▸ " };
    let lines = format!("  lines {}–{}", section.range.from + 1, section.range.to);
    match section.kind {
        SectionKind::Step(number) => {
            let step = job.steps.iter().find(|step| step.number == number);
            let (mark, style) = step.map_or(("·", theme.dim()), |step| {
                let outcome = step.outcome();
                (
                    mark(outcome.kind),
                    theme.tone(crate::human::outcome_tone(outcome.kind)),
                )
            });
            let took = step
                .and_then(|step| step.took(now))
                .map(|took| format!("  {}", duration(took)))
                .unwrap_or_default();
            let name = step.map_or_else(|| format!("step {number}"), |step| step.name.clone());
            let head = format!("{fold}{mark} {number:>2} ");
            let room = width.saturating_sub(head.width() + took.width() + lines.width());
            Line::from(vec![
                Span::styled(fold.to_owned(), bold()),
                Span::styled(format!("{mark} "), style),
                Span::styled(format!("{number:>2} "), theme.dim()),
                Span::styled(fit(&name, room.min(name.width())), bold()),
                Span::raw(took),
                Span::styled(lines, theme.dim()),
            ])
        }
        SectionKind::Unattributed | SectionKind::Flat => Line::from(vec![
            Span::styled(fold.to_owned(), bold()),
            Span::styled(lines.trim_start().to_owned(), theme.dim()),
        ]),
    }
}

/// A step's outcome as one glyph.
fn mark(kind: api::OutcomeKind) -> &'static str {
    match kind {
        api::OutcomeKind::Succeeded => "✓",
        api::OutcomeKind::Failed | api::OutcomeKind::RunnerFault => "✗",
        api::OutcomeKind::Running => "●",
        api::OutcomeKind::Waiting => "○",
        api::OutcomeKind::Ended => "-",
    }
}

/// A group's header, indented by its depth: open or closed, and its title.
fn group_line(
    outline: Option<&LogOutline>,
    index: usize,
    open: bool,
    gutter: usize,
    width: usize,
    _theme: Theme,
) -> Line<'static> {
    let Some(group) = outline.and_then(|outline| outline.groups.get(index)) else {
        return Line::default();
    };
    let indent = gutter + 1 + 2 * usize::try_from(group.depth).unwrap_or(0);
    let fold = if open { "▾ " } else { "▸ " };
    Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(
            format!(
                "{fold}{}",
                fit(&group.title, width.saturating_sub(indent + 2)).trim_end()
            ),
            bold(),
        ),
    ])
}

/// A line: its number, dim, then its runs in core's colours, a match reversed, and what is not shown of it.
pub(super) fn styled_line(
    styled: &StyledLine,
    gutter: usize,
    width: usize,
    theme: Theme,
) -> Line<'static> {
    let base = match styled.kind {
        LineKind::Error => theme.tone(Tone::Bad),
        LineKind::Warning => theme.wait(),
        LineKind::Group => bold(),
        LineKind::Notice => theme.cpu(),
        LineKind::Debug | LineKind::Command | LineKind::EndGroup => theme.dim(),
        LineKind::Output | LineKind::Unknown => Style::new(),
    };
    let mut spans = vec![Span::styled(
        format!("{:>gutter$} ", styled.line + 1),
        theme.dim(),
    )];
    let mut used = gutter + 1;
    for run in &styled.runs {
        if used >= width {
            break;
        }
        let mut style = match run.color {
            Some(colour) => colour_of(colour, theme),
            None => base,
        };
        if run.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if run.mark {
            style = style.add_modifier(Modifier::REVERSED);
        }
        // Tabs as the terminal would, roughly: a tab is four spaces.
        let text = run.text.replace('\t', "    ");
        let room = width - used;
        let text = if text.width() > room {
            fit(&text, room)
        } else {
            text
        };
        used += text.width();
        spans.push(Span::styled(text, style));
    }
    if let Some(omitted) = styled.omitted_words() {
        spans.push(Span::styled(format!(" … {omitted}"), theme.dim()));
    }
    Line::from(spans)
}

/// Core's colour of a run as the terminal's.
fn colour_of(colour: api::Color, theme: Theme) -> Style {
    let colour = match colour {
        api::Color::Red => Color::LightRed,
        api::Color::Green => Color::LightGreen,
        api::Color::Yellow => Color::LightYellow,
        api::Color::Blue => Color::LightBlue,
        api::Color::Magenta => Color::LightMagenta,
        api::Color::Cyan => Color::LightCyan,
        api::Color::Grey => return theme.dim(),
        api::Color::Unknown => return Style::new(),
    };
    theme.fg(colour)
}
