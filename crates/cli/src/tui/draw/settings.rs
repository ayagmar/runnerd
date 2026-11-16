//! The settings view: the config's path and what its check says, then the doctor's checks as a list of marks, what
//! fails first and the passing checks folded into one row, and the selected check whole: beside the list on a wide
//! terminal, under it on a narrow one. Editing the config is not the TUI's.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{self, Check, CheckStatus, Doctor};
use unicode_width::UnicodeWidthStr;

use super::{Theme, beside, bold, clip, fit, heading, length, picked, wrap};
use crate::human::Tone;
use crate::tui::app::{App, CheckRow, ConfigRead, Fold};
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

/// The rows the selected check keeps under a list that scrolls, on a narrow terminal.
const UNDER_LEAST: usize = 6;

pub(super) fn settings(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let (main, pane) = beside(frame, area, theme);
    let width = usize::from(main.width);
    let config = config_lines(&app.config, width, theme);
    let rows = app.check_rows();
    let selected_check = match (&app.doctor, rows.get(app.check)) {
        (Reading::Read(doctor), Some(CheckRow::Check(at))) => doctor.checks.get(*at),
        _ => None,
    };
    let under = match (pane, selected_check) {
        (None, Some(check)) => check_lines(check, width, theme),
        _ => Vec::new(),
    };
    // The doctor's title and its rows come first, then the selected check whole: the config's block gives way to
    // them down to its path and mark, and the list scrolls to leave the check at least `UNDER_LEAST` rows.
    let height = usize::from(main.height);
    let list = rows.len().max(1);
    let under_wanted = if under.is_empty() { 0 } else { under.len() + 1 };
    let config_least = config.len().min(2);
    let config_rows = config
        .len()
        .min(height.saturating_sub(1 + list + under_wanted))
        .max(config_least.min(height));
    let left = height.saturating_sub(config_rows + 1);
    let list_rows = list.min(
        left.saturating_sub(under_wanted.min(UNDER_LEAST))
            .max(left.min(1)),
    );
    let under_rows = left - list_rows;
    let config = if config_rows < config.len() {
        config_short(&app.config, config, config_rows, theme)
    } else {
        config
    };
    let [config_area, doctor_title, checks_area, under_area] = Layout::vertical([
        Constraint::Length(length(config_rows)),
        Constraint::Length(1),
        Constraint::Length(length(list_rows)),
        Constraint::Length(length(under_rows)),
    ])
    .areas(main);
    frame.render_widget(Paragraph::new(config), config_area);
    let doctor = match &app.doctor {
        Reading::Read(doctor) => doctor,
        Reading::Unread(why) => {
            heading(frame, doctor_title, "Doctor", String::new(), theme);
            frame.render_widget(
                Line::styled(unread(why, api::Feature::Doctor), theme.dim()),
                checks_area,
            );
            return;
        }
    };
    let state = doctor
        .state
        .as_ref()
        .and_then(|state| state.line(now))
        .unwrap_or_default();
    heading(frame, doctor_title, "Doctor", state, theme);
    checks(frame, checks_area, app, doctor, &rows, theme, hits);
    if under_rows > 0 {
        let mut lines = vec![Line::from("")];
        lines.extend(clip(under, under_rows - 1, theme));
        frame.render_widget(Paragraph::new(lines), under_area);
    }
    if let (Some(pane), Some(check)) = (pane, selected_check) {
        let lines = check_lines(check, usize::from(pane.width), theme);
        frame.render_widget(
            Paragraph::new(clip(lines, usize::from(pane.height), theme)),
            pane,
        );
    }
}

/// A check's mark: a glyph that reads without colour, beside its label.
fn mark(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "✓",
        CheckStatus::Fail => "✗",
        CheckStatus::Warn => "▲",
        CheckStatus::Skip => "–",
        CheckStatus::Info => "·",
    }
}

/// A check whole: its mark and name, what it found, and what fixes it, each wrapped to `width`.
fn check_lines(check: &Check, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let tone = theme.of(check.status.tone());
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{} {} ", mark(check.status), check.status.label()),
            tone,
        ),
        Span::styled(check.name.clone(), bold()),
    ])];
    lines.extend(wrap(&check.detail, width).into_iter().map(Line::from));
    if let Some(fix) = &check.fix {
        lines.push(Line::from(""));
        lines.extend(
            fix.lines()
                .flat_map(|line| wrap(line, width.saturating_sub(2)))
                .map(|line| Line::styled(format!("  {line}"), bold())),
        );
    }
    lines
}

/// The config's lines in `rows` short of room for them all: its path, then its check's mark and word with how many
/// lines do not fit, so whether it is valid still shows.
fn config_short(
    config: &Reading<ConfigRead>,
    lines: Vec<Line<'static>>,
    rows: usize,
    theme: Theme,
) -> Vec<Line<'static>> {
    let Reading::Read(read) = config else {
        return clip(lines, rows, theme);
    };
    if rows < 2 {
        return lines.into_iter().take(rows).collect();
    }
    let (status, tone) = if read.check.valid {
        (CheckStatus::Pass, Tone::Good)
    } else {
        (CheckStatus::Fail, Tone::Bad)
    };
    // The path, and the blank line that ends the block, are not counted.
    let hidden = lines.len().saturating_sub(2);
    let mut short: Vec<Line<'static>> = lines.into_iter().take(1).collect();
    short.push(Line::from(vec![
        Span::styled(
            format!("{} {}", mark(status), status.label()),
            theme.tone(tone),
        ),
        Span::styled(format!("  … {hidden} more lines"), theme.dim()),
    ]));
    short
}

/// What a read of `feature` that gave nothing says.
fn unread(why: &Unread, feature: api::Feature) -> String {
    match why {
        Unread::Waiting => "reading…".to_owned(),
        Unread::Unsupported => feature.unsupported(),
        Unread::Failed(why) => why.clone(),
    }
}

/// The config's path, its check's message in its tone behind a mark, and each warning; a blank line after.
fn config_lines(config: &Reading<ConfigRead>, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let read = match config {
        Reading::Read(read) => read,
        Reading::Unread(why) => {
            return vec![
                Line::styled(api::Section::Config.title(), bold()),
                Line::styled(unread(why, api::Feature::Config), theme.dim()),
                Line::from(""),
            ];
        }
    };
    let title = api::Section::Config.title();
    let path = fit(&read.file.path, width.saturating_sub(title.width() + 2));
    let mut lines = vec![Line::from(vec![
        Span::styled(title, bold()),
        Span::raw(format!("  {}", path.trim_end())),
    ])];
    let (status, tone) = if read.check.valid {
        (CheckStatus::Pass, Tone::Good)
    } else {
        (CheckStatus::Fail, Tone::Bad)
    };
    let marked = |status: CheckStatus, text: &str, style| -> Vec<Line<'static>> {
        wrap(text, width.saturating_sub(2))
            .into_iter()
            .enumerate()
            .map(|(at, line)| {
                let head = if at == 0 { mark(status) } else { " " };
                Line::from(vec![
                    Span::styled(format!("{head} "), style),
                    Span::styled(line, style),
                ])
            })
            .collect()
    };
    lines.extend(marked(status, &read.check.message, theme.tone(tone)));
    for warning in &read.check.warnings {
        lines.extend(marked(CheckStatus::Warn, warning, theme.wait()));
    }
    lines.push(Line::from(""));
    lines
}

/// A row per check in the order of `rows`: its mark and label in its tone, its name, and what it found, cut, as the
/// selected one shows whole; the passing checks folded show how many and their names.
fn checks(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    doctor: &Doctor,
    rows: &[CheckRow],
    theme: Theme,
    hits: &mut Hits,
) {
    let width = usize::from(area.width);
    let name_width = doctor
        .checks
        .iter()
        .map(|check| check.name.width())
        .max()
        .unwrap_or(0)
        .min(width / 3);
    let label_width = 2 + 5;
    let shown = usize::from(area.height);
    let first = app.check.saturating_sub(shown.saturating_sub(1));
    let rest = width.saturating_sub(label_width + 1 + name_width + 2);
    let lines: Vec<Line<'_>> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, row)| {
            let line = match row {
                CheckRow::Check(index) => {
                    let check = &doctor.checks[*index];
                    let tone = theme.of(check.status.tone());
                    let indent = if check.status == CheckStatus::Pass {
                        "  "
                    } else {
                        ""
                    };
                    Line::from(vec![
                        Span::styled(
                            format!(
                                "{indent}{} {:<5} ",
                                mark(check.status),
                                check.status.label()
                            ),
                            tone,
                        ),
                        Span::styled(
                            format!(
                                "{}  ",
                                fit(&check.name, name_width.saturating_sub(indent.len()))
                            ),
                            bold(),
                        ),
                        Span::raw(fit(&check.detail, rest).trim_end().to_owned()),
                    ])
                }
                CheckRow::Passing(count) => {
                    let names: Vec<&str> = doctor
                        .checks
                        .iter()
                        .filter(|check| check.status == CheckStatus::Pass)
                        .map(|check| check.name.as_str())
                        .collect();
                    let fold = match app.passing {
                        Fold::Folded => "▸",
                        Fold::Open => "▾",
                    };
                    let tone = theme.of(CheckStatus::Pass.tone());
                    Line::from(vec![
                        Span::styled(
                            format!(
                                "{} {:<5} ",
                                mark(CheckStatus::Pass),
                                CheckStatus::Pass.label()
                            ),
                            tone,
                        ),
                        Span::styled(
                            format!("{:<name_width$}  ", format!("{fold} {count}")),
                            bold(),
                        ),
                        Span::styled(
                            fit(&names.join(", "), rest).trim_end().to_owned(),
                            theme.dim(),
                        ),
                    ])
                }
            };
            if at == app.check { picked(line) } else { line }
        })
        .collect();
    hits.rows(
        area,
        List::Checks,
        first,
        rows.len() - first.min(rows.len()),
    );
    frame.render_widget(Paragraph::new(lines), area);
}
