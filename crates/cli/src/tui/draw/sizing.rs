//! The sizing view: what each profile's jobs use, the advice, and what auto sizing does next.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use runnerd_core::api::{self, ProfileSizing, Sizing, job_title};
use runnerd_core::config::ByteSize;
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::job::fit_details;
use super::{bold, fit, heading, length, selected, widths, wrap};
use crate::human::{self, Tone, bytes, cores_used, duration};
use crate::tui::app::App;

/// What each profile's jobs use and what to set its memory and slots to; the selected profile's jobs below. Under
/// 100 columns the medians, the jobs at their cap, the largest peaks and the waits of warm and cold starts are left
/// out.
pub(super) fn sizing(frame: &mut Frame<'_>, area: Rect, app: &App, now: u64, theme: Theme) {
    let Some(sizing) = &app.sizing else {
        let [title, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        heading(
            frame,
            title,
            api::View::Sizing.title(),
            String::new(),
            theme,
        );
        frame.render_widget(Line::styled("reading…", theme.dim()), rest);
        return;
    };
    let wide = area.width >= 100;
    let profiles = length(sizing.profiles.len());
    let plan = auto_plan(sizing, usize::from(area.width), now, app, theme);
    let [title, profiles_area, plan_area, advice_area, jobs_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(profiles.saturating_add(1)),
        Constraint::Length(length(plan.len())),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(plan), plan_area);
    heading(
        frame,
        title,
        api::View::Sizing.title(),
        fit_details(
            [(0, format!("{} of history", duration(sizing.covered())))]
                .into_iter()
                .chain([1, 2, 2].into_iter().zip(sizing.method_parts()))
                .chain([(
                    if sizing.auto_plan.is_some() { 3 } else { 0 },
                    format!("auto sizing {}", if sizing.auto { "on" } else { "off" }),
                )])
                .collect(),
            usize::from(title.width).saturating_sub(api::View::Sizing.title().width() + 2),
        ),
        theme,
    );
    sizing_profiles(frame, profiles_area, app, sizing, theme);
    let Some(profile) = sizing.profiles.get(app.sized) else {
        return;
    };
    let width = usize::from(advice_area.width);
    // The advice wraps onto a second line rather than lose its end; the hint below it is cut.
    let mut lines = vec![Line::styled(
        format!("{}: {}", profile.name, sizing.advice_line(profile)),
        theme.tone(profile.advice.tone().into()),
    )];
    let last = sizing
        .changes
        .iter()
        .rfind(|change| change.profile == profile.name);
    if let Some(change) = profile.advice.change().filter(|_| !sizing.auto) {
        lines.push(Line::styled(
            fit(
                &format!(
                    "a writes memory = \"{}\", slots = {} into the config; the daemon checks it first",
                    ByteSize(change.memory),
                    change.slots
                ),
                width,
            ),
            theme.dim(),
        ));
    } else if let Some(change) = last {
        lines.push(Line::styled(
            fit(
                &format!(
                    "auto sizing {}: {}; runnerctl sizing undo {} writes it back",
                    human::ago(change.at, now),
                    change.line(),
                    profile.name
                ),
                width,
            ),
            theme.dim(),
        ));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), advice_area);
    sizing_jobs(frame, jobs_area, profile, wide, theme);
}

/// Under auto sizing, its rules, why no look changes anything if one would not, and per profile what its next look
/// changes, how long a recent change holds it, or that its advice asks for nothing; a blank line after. Nothing while
/// auto sizing is off, or from a daemon that does not say what it does next.
pub(super) fn auto_plan(
    sizing: &Sizing,
    width: usize,
    now: u64,
    app: &App,
    theme: Theme,
) -> Vec<Line<'static>> {
    let Some(plan) = sizing.auto_plan.as_ref().filter(|_| sizing.auto) else {
        return Vec::new();
    };
    let mut lines = vec![Line::from("")];
    // The rules and what blocks every look are sentences, wrapped rather than cut.
    for (position, line) in wrap(&format!("Auto sizing {}", plan.rules()), width)
        .into_iter()
        .enumerate()
    {
        lines.push(
            match line.strip_prefix("Auto sizing ").filter(|_| position == 0) {
                Some(rest) => Line::from(vec![
                    Span::styled("Auto sizing ", bold()),
                    Span::styled(rest.to_owned(), theme.dim()),
                ]),
                None => Line::styled(line, theme.dim()),
            },
        );
    }
    if let Some(blocked) = plan.blocked_line() {
        lines.extend(
            wrap(&blocked, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.tone(Tone::Bad))),
        );
    }
    let name_width = sizing
        .profiles
        .iter()
        .map(|profile| profile.name.width())
        .max()
        .unwrap_or(0);
    for profile in &sizing.profiles {
        let (what, why) = human::plan_line(sizing, plan, profile, now);
        let room = width.saturating_sub(2 + name_width + 1);
        let what_width = what.width().min(room);
        let mut spans = vec![
            Span::styled("■ ", theme.profile(app.profile_index(&profile.name))),
            Span::raw(format!("{:<name_width$} ", profile.name)),
            Span::raw(fit(&what, what_width)),
        ];
        let rest = room.saturating_sub(what_width + 2);
        if rest >= 12 {
            spans.push(Span::styled(
                format!("  {}", fit(&why, rest).trim_end()),
                theme.dim(),
            ));
        }
        lines.push(Line::from(spans));
        // Beside the change, which can still be to its memory or slots.
        // A sentence with its figures and its reason, wrapped rather than cut.
        for refusal in plan.refusals(&profile.name) {
            let indent = 2 + name_width + 1;
            lines.extend(
                wrap(&refusal, width.saturating_sub(indent))
                    .into_iter()
                    .map(|line| {
                        Line::styled(format!("{:indent$}{line}", ""), theme.tone(Tone::Busy))
                    }),
            );
        }
    }
    lines
}

/// A row per profile: its memory and slots, its jobs' peaks, how busy and how long waited, the advice.
pub(super) fn sizing_profiles(
    frame: &mut Frame<'_>,
    profiles_area: Rect,
    app: &App,
    sizing: &Sizing,
    theme: Theme,
) {
    let wide = profiles_area.width >= 100;
    // The percentiles' runs: each job's recent ones, fewer than the jobs the waits are over.
    let mut columns = vec![("PROFILE", 0), ("MEMORY", 10), ("RUNS", 5)];
    // Narrow, the largest peak advice is sized for stays and the percentiles go.
    if wide {
        columns.push(("P50", 6));
    }
    columns.push(("P95", 6));
    if wide {
        columns.push(("P99", 6));
    }
    columns.push(("MAX", 6));
    if wide {
        columns.push(("AT CAP", 6));
    }
    columns.extend([("CPU P99", 7), ("BUSY", 5), ("WAIT P95", 8)]);
    if wide {
        columns.push(("P50 WARM/COLD", 13));
    }
    columns.push(("ADVICE", 13));
    let rows = sizing.profiles.iter().map(|profile| {
        let peaks = &profile.peaks;
        let mut cells = vec![
            Cell::from(Span::styled(
                profile.name.clone(),
                theme.profile(app.profile_index(&profile.name)),
            )),
            Cell::from(format!("{} × {}", bytes(profile.memory), profile.slots)),
            Cell::from(peaks.jobs.to_string()),
        ];
        if wide {
            cells.push(Cell::from(bytes(peaks.memory.p50)));
        }
        cells.push(Cell::from(bytes(peaks.memory.p95)));
        if wide {
            cells.push(Cell::from(bytes(peaks.memory.p99)));
        }
        cells.push(Cell::from(bytes(peaks.memory.max)));
        if wide {
            cells.push(Cell::from(if peaks.at_cap > 0 {
                Span::styled(peaks.at_cap.to_string(), theme.tone(Tone::Bad))
            } else {
                Span::raw("")
            }));
        }
        cells.extend([
            Cell::from(cpu_cell(peaks)),
            Cell::from(format!("{}%", profile.busy)),
            Cell::from(duration(profile.wait.p95)),
        ]);
        if wide {
            let starts = &profile.starts;
            cells.push(Cell::from(if starts.warm > 0 {
                format!(
                    "{} / {}",
                    duration(starts.warm_p50),
                    duration(starts.cold_p50)
                )
            } else {
                String::new()
            }));
        }
        cells.push(Cell::from(Span::styled(
            profile.advice_word(),
            theme.tone(profile.advice.tone().into()),
        )));
        Row::new(cells)
    });
    let table = Table::new(rows, widths(&columns))
        .header(Row::new(columns.iter().map(|(name, _)| *name)).style(theme.dim()))
        .row_highlight_style(selected());
    let mut state = TableState::default().with_selected(Some(app.sized));
    frame.render_stateful_widget(table, profiles_area, &mut state);
}

/// The CPU p99 of the runs at the profile's `cpus` now, `–` without one.
pub(super) fn cpu_cell(peaks: &api::Peaks) -> String {
    peaks.cpu_p99().map_or_else(|| "–".to_owned(), cores_used)
}

/// The selected profile's jobs, a row each.
pub(super) fn sizing_jobs(
    frame: &mut Frame<'_>,
    jobs_area: Rect,
    profile: &ProfileSizing,
    wide: bool,
    theme: Theme,
) {
    let mut columns = vec![("REPOSITORY", 0), ("JOB", 0), ("RUNS", 5)];
    columns.push(("P95", 6));
    if wide {
        columns.push(("P99", 6));
    }
    columns.extend([("MAX", 6), ("CPU P99", 7), ("WAIT P95", 8), ("NEEDS", 11)]);
    // The repository and the job share what the other columns and the spaces between them leave.
    let fixed: usize = columns.iter().map(|(_, width)| width + 1).sum::<usize>() - 1;
    let shared = usize::from(jobs_area.width).saturating_sub(fixed);
    let repository_width = shared / 2;
    let rows = profile.jobs.iter().map(|job| {
        let needs = job.needs();
        let needs = Span::styled(needs.text, theme.tone(needs.tone.into()));
        let name = job_title(job.workflow.as_deref(), &job.name, None).to_string();
        let mut cells = vec![
            Cell::from(fit(&job.repository, repository_width)),
            Cell::from(fit(&name, shared - repository_width)),
            Cell::from(job.peaks.jobs.to_string()),
            Cell::from(bytes(job.peaks.memory.p95)),
        ];
        if wide {
            cells.push(Cell::from(bytes(job.peaks.memory.p99)));
        }
        cells.extend([
            Cell::from(bytes(job.peaks.memory.max)),
            Cell::from(cpu_cell(&job.peaks)),
            Cell::from(duration(job.wait.p95)),
            Cell::from(needs),
        ]);
        Row::new(cells)
    });
    let mut constraints = widths(&columns);
    constraints[0] = Constraint::Length(u16::try_from(repository_width).unwrap_or(0));
    let table = Table::new(rows, constraints)
        .header(Row::new(columns.iter().map(|(name, _)| *name)).style(theme.dim()));
    frame.render_widget(table, jobs_area);
}
