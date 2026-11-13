//! The settings view: the config's path and what its check says, then the doctor's checks with the state of its last
//! run, the selected check's fix under them. Editing the config is not the TUI's.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{self, Doctor};
use unicode_width::UnicodeWidthStr;

use super::{Theme, bold, fit, heading, length, selected, wrap};
use crate::human::Tone;
use crate::tui::app::{App, ConfigRead};
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

pub(super) fn settings(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let width = usize::from(area.width);
    let config = config_lines(&app.config, width, theme);
    let [config_area, doctor_title, checks_area, fix_area] = Layout::vertical([
        Constraint::Length(length(config.len())),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(4),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(config), config_area);
    let doctor = match &app.doctor {
        Reading::Read(doctor) => doctor,
        Reading::Unread(why) => {
            heading(frame, doctor_title, "Doctor", String::new(), theme);
            frame.render_widget(Line::styled(unread(why), theme.dim()), checks_area);
            return;
        }
    };
    let state = doctor
        .state
        .as_ref()
        .and_then(|state| state.line(now))
        .unwrap_or_default();
    heading(frame, doctor_title, "Doctor", state, theme);
    checks(frame, checks_area, app, doctor, theme, hits);
    // The selected check's fix, whole.
    if let Some(fix) = doctor
        .checks
        .get(app.check)
        .and_then(|check| check.fix.as_deref())
    {
        let lines: Vec<Line<'_>> = fix
            .lines()
            .flat_map(|line| wrap(line, width.saturating_sub(2)))
            .map(|line| Line::from(format!("  {line}")))
            .collect();
        frame.render_widget(Paragraph::new(lines), fix_area);
    }
}

/// What a read that gave nothing says.
fn unread(why: &Unread) -> String {
    match why {
        Unread::Waiting => "reading…".to_owned(),
        Unread::Unsupported => api::NOT_SUPPORTED.to_owned(),
        Unread::Failed(why) => why.clone(),
    }
}

/// The config's path, its check's message in its tone, and each warning; a blank line after.
fn config_lines(config: &Reading<ConfigRead>, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let read = match config {
        Reading::Read(read) => read,
        Reading::Unread(why) => {
            return vec![
                Line::styled(api::Section::Config.title(), bold()),
                Line::styled(unread(why), theme.dim()),
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
    let tone = if read.check.valid {
        Tone::Good
    } else {
        Tone::Bad
    };
    lines.extend(
        wrap(&read.check.message, width)
            .into_iter()
            .map(|line| Line::styled(line, theme.tone(tone))),
    );
    for warning in &read.check.warnings {
        lines.extend(
            wrap(warning, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.wait())),
        );
    }
    lines.push(Line::from(""));
    lines
}

/// A row per check: its status in its tone, its name, and what it found.
fn checks(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    doctor: &Doctor,
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
    let shown = usize::from(area.height);
    let first = app.check.saturating_sub(shown.saturating_sub(1));
    let lines: Vec<Line<'_>> = doctor
        .checks
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, check)| {
            let status = check.status.label();
            let rest = width.saturating_sub(6 + name_width + 2);
            let line = Line::from(vec![
                Span::styled(format!("{status:<5} "), theme.of(check.status.tone())),
                Span::styled(format!("{}  ", fit(&check.name, name_width)), bold()),
                Span::raw(fit(&check.detail, rest).trim_end().to_owned()),
            ]);
            if at == app.check {
                line.style(selected())
            } else {
                line
            }
        })
        .collect();
    hits.rows(
        area,
        List::Checks,
        first,
        doctor.checks.len() - first.min(doctor.checks.len()),
    );
    frame.render_widget(Paragraph::new(lines), area);
}
