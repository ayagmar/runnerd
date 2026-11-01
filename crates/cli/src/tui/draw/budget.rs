//! The overview's budget: the bar of what the slice reserves, slot by slot, and the host's figures under it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use runnerd_core::api::{BudgetParts, HeldKind, HeldPart, Status};
use runnerd_core::config::ByteSize;
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::job::fit_details;
use super::slots::slot_job;
use super::{fit, heading};
use crate::human::bytes;
use crate::tui::app::App;

/// `part` of `whole` as cells of `width`.
pub(super) fn scale(part: u64, whole: u64, width: u16) -> u16 {
    let cells = u128::from(part) * u128::from(width) / u128::from(whole.max(1));
    u16::try_from(cells).unwrap_or(width).min(width)
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

/// The budget as one bar: a block per slot as wide as its memory (under measured admission, a block per runner as
/// wide as what it reserves), filled as far as its job uses it; then the mirror and each seed container, then what
/// is free and what is closed.
pub(super) fn budget(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let status = &app.live.status;
    let mut note = budget_note(app);
    let [title, bar, labels] = Layout::vertical([Constraint::Length(1); 3]).areas(area);

    let parts = status.budget_parts();
    let blocks: Vec<_> = parts
        .slots
        .iter()
        .filter_map(|part| {
            let profile = status.profiles.get(part.profile)?;
            let slot = profile.slots.iter().find(|slot| slot.slot == part.slot)?;
            Some((part.profile, profile, slot, part.memory))
        })
        .collect();
    let whole = parts.whole;
    let mut cells = Vec::new();
    let mut names = Vec::new();
    let mut drawn = 0;
    for (index, profile, slot, memory) in blocks {
        let width = scale(memory, whole, bar.width).max(1);
        let style = theme.profile(Some(index));
        let sample =
            slot_job(app, profile, slot).and_then(|job| app.live.samples.get(&job.summary.id));
        let used_cells = sample.map_or(0, |sample| scale(sample.memory, memory, width));
        let name = format!("{} {}", profile.name, slot.slot);
        let (text, labels) = if slot.runner.is_some() {
            let figure = sample.map_or(memory, |sample| sample.memory);
            let full = bytes(figure);
            let short = ByteSize(figure).short();
            let used = if sample.is_some() { " used" } else { "" };
            (
                format!(
                    "{}{}",
                    "█".repeat(usize::from(used_cells)),
                    "▓".repeat(usize::from(width - used_cells))
                ),
                vec![
                    format!("{name} {full}{used}"),
                    format!("{name} {full}"),
                    format!("{name} {short}"),
                    name.clone(),
                ],
            )
        } else {
            (
                "░".repeat(usize::from(width)),
                vec![format!("{name} idle"), name.clone()],
            )
        };
        // A gap between blocks, taken from the block itself.
        let mut text: String = text
            .chars()
            .take(usize::from(width.saturating_sub(1)))
            .collect();
        text.push(' ');
        cells.push(Span::styled(text, style));
        let room = usize::from(width.saturating_sub(1));
        // Short of room, `used` goes, then the figure shortens, then it goes whole: a figure is never cut.
        let label = labels
            .into_iter()
            .find(|label| label.width() <= room)
            .unwrap_or_default();
        names.push(Span::styled(format!("{} ", fit(&label, room)), style));
        drawn += width;
    }
    // The mirror and each seed container, named as the slots are, or in the heading when too narrow for it.
    for Held {
        style,
        label,
        memory,
    } in held(status, &parts.held, theme)
    {
        let width = scale(memory, whole, bar.width)
            .max(2)
            .min(bar.width.saturating_sub(drawn));
        let room = usize::from(width.saturating_sub(1));
        if label.width() > room {
            note.push((3, label.clone()));
        }
        if width < 2 {
            continue;
        }
        let mut text = "▒".repeat(room);
        text.push(' ');
        cells.push(Span::styled(text, style));
        let name = if label.width() <= room {
            label
        } else {
            String::new()
        };
        names.push(Span::styled(format!("{name:room$} "), style));
        drawn += width;
    }
    let rest = bar_rest(&parts, bar.width, drawn, theme);
    cells.extend(rest.cells);
    names.extend(rest.names);
    note.extend(rest.note.map(|label| (3, label)));
    let room = usize::from(title.width).saturating_sub("Budget".width() + 2);
    heading(frame, title, "Budget", fit_details(note, room), theme);
    frame.render_widget(Line::from(cells), bar);
    frame.render_widget(Line::from(names), labels);
}

/// The bar past its blocks: what is free, then what is closed, hatched and named under it or, too narrow for that,
/// in the heading as the mirror is.
pub(super) struct Rest {
    cells: Vec<Span<'static>>,
    names: Vec<Span<'static>>,
    note: Option<String>,
}

/// The bar of `width` cells past the `drawn` ones, for what of `parts` is free and closed. The closed part
/// takes what rounding leaves, so the bar never shows more free than core says is.
pub(super) fn bar_rest(parts: &BudgetParts, width: u16, drawn: u16, theme: Theme) -> Rest {
    let (free, closed, whole) = (parts.free.as_ref(), parts.closed.as_ref(), parts.whole);
    let rest = width.saturating_sub(drawn);
    let free_cells = match (free, closed) {
        (Some(free), Some(_)) => scale(free.memory, whole, width).min(rest.saturating_sub(1)),
        (Some(_), None) => rest,
        (None, _) => 0,
    };
    let closed = closed.map(|closed| {
        (
            rest - free_cells,
            format!("{} {}", closed.kind.name(), closed.reading.figure),
        )
    });
    let blank = if free.is_none() && closed.is_none() {
        rest
    } else {
        0
    };
    let mut out = Rest {
        cells: vec![
            Span::styled("·".repeat(usize::from(free_cells)), theme.dim()),
            Span::raw(" ".repeat(usize::from(blank))),
        ],
        names: Vec::new(),
        note: None,
    };
    if let Some((width, label)) = closed {
        out.cells
            .push(Span::styled("╱".repeat(usize::from(width)), theme.dim()));
        out.names
            .push(Span::raw(" ".repeat(usize::from(free_cells))));
        if label.width() <= usize::from(width) {
            out.names.push(Span::styled(label, theme.dim()));
        } else {
            out.note = Some(label);
        }
    }
    out
}

/// Memory the budget holds for something other than a slot.
pub(super) struct Held {
    style: Style,
    label: String,
    memory: u64,
}

/// The mirror, each seed container in its profile's colour, then each shell open in a record.
pub(super) fn held(status: &Status, held: &[HeldPart], theme: Theme) -> Vec<Held> {
    held.iter()
        .map(|part| {
            let style = match &part.kind {
                HeldKind::Seed { profile } => theme.profile(
                    status
                        .profiles
                        .iter()
                        .position(|status| &status.name == profile),
                ),
                HeldKind::Mirror | HeldKind::Shell { .. } => Style::default(),
            };
            Held {
                style,
                label: format!("{} {}", part.name(), bytes(part.memory)),
                memory: part.memory,
            }
        })
        .collect()
}
