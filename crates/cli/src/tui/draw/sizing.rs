//! The sizing view: what each profile's jobs use, the advice, and what auto sizing does next.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Table, TableState};
use runnerd_core::api::{self, ProfileSizing, Sizing, job_title};
use runnerd_core::config::ByteSize;
use unicode_width::UnicodeWidthStr;

use super::Theme;
use super::budget::scale;
use super::job::fit_details;
use super::{TableColumn, beside, bold, clip, fit, fitted_table, heading, length, wrap};
use crate::human::{self, Tone, bytes, cores_used, duration};
use crate::tui::app::App;
use crate::tui::mouse::{Hits, List};

/// What each profile's jobs use and what to set its memory and slots to, then the selected profile's jobs, and what
/// the selected profile's advice and auto sizing mean for it: beside the tables on a wide terminal, between them on a
/// narrow one. Under auto sizing, its rules and what its next look does to each profile follow the profiles. Columns
/// are as wide as their content; short of room the runs with a peak, the medians, the warm starts, the jobs at their
/// cap, the jobs and the p99s give way first, then the warm and cold starts' p50s, how busy the slots were and the
/// waits; a column that still does not fit goes whole, never cut.
pub(super) fn sizing(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let Some(sizing) = &app.sizing else {
        heading(
            frame,
            title,
            api::Section::Sizing.title(),
            String::new(),
            theme,
        );
        frame.render_widget(Line::styled("reading…", theme.dim()), body);
        return;
    };
    heading(
        frame,
        title,
        api::Section::Sizing.title(),
        fit_details(
            [(0, format!("{} of history", duration(sizing.covered())))]
                .into_iter()
                .chain([1, 2, 2].into_iter().zip(sizing.method_parts()))
                .chain([(
                    if sizing.auto_plan.is_some() { 3 } else { 0 },
                    format!("auto sizing {}", if sizing.auto { "on" } else { "off" }),
                )])
                .collect(),
            usize::from(title.width).saturating_sub(api::Section::Sizing.title().width() + 2),
        ),
        theme,
    );
    let (main, pane) = beside(frame, body, theme);
    let width = usize::from(main.width);
    let profile = sizing.profiles.get(app.sized);
    let under = match (profile, pane) {
        (Some(profile), None) => meaning(sizing, profile, width, now, app, theme),
        _ => Vec::new(),
    };
    let jobs = profile.map_or(0, |profile| profile.jobs.len());
    let profiles_height = length(sizing.profiles.len() + 1);
    let rows = Rows::of(
        main.height.saturating_sub(profiles_height),
        jobs,
        under.len(),
        rules(sizing, width, theme),
        plan_rows(sizing, width, now, app, theme),
    );
    let [profiles_area, plan_area, under_area, jobs_area] = Layout::vertical([
        Constraint::Length(profiles_height),
        Constraint::Length(rows.plan_height),
        Constraint::Length(rows.under),
        Constraint::Length(rows.jobs),
    ])
    .areas(main);
    let table = profiles_table(app, sizing, width, theme);
    let mut state = TableState::default().with_selected(Some(app.sized));
    frame.render_stateful_widget(table, profiles_area, &mut state);
    super::table_hits(hits, profiles_area, List::Sizing, 0, sizing.profiles.len());
    let mut lines = vec![Line::from("")];
    lines.extend(rows.plan);
    frame.render_widget(Paragraph::new(lines), plan_area);
    let mut lines = vec![Line::from("")];
    lines.extend(clip(
        under,
        usize::from(rows.under.saturating_sub(1)),
        theme,
    ));
    frame.render_widget(Paragraph::new(lines), under_area);
    if let Some(profile) = profile {
        if rows.jobs > 0 {
            let [_, table_area] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(jobs_area);
            frame.render_widget(jobs_table(profile, width, theme), table_area);
        }
        if let Some(pane) = pane {
            let pane_width = usize::from(pane.width);
            let mut lines = meaning(sizing, profile, pane_width, now, app, theme);
            lines.push(Line::from(""));
            lines.extend(peaks(profile, pane_width, app, theme));
            let lines = clip(lines, usize::from(pane.height), theme);
            frame.render_widget(Paragraph::new(lines), pane);
        }
    }
}

/// How the rows under the profiles are shared, from first to last given: the selected profile's jobs keep a blank row,
/// their header and two jobs, drawn only with a job in sight so an empty table never reads as a profile without
/// jobs; then the selected profile's meaning; then what auto sizing does to each profile, and its rules, each whole
/// or not at all; the jobs take the rest.
struct Rows {
    plan: Vec<Line<'static>>,
    plan_height: u16,
    under: u16,
    jobs: u16,
}

impl Rows {
    fn of(
        mut left: u16,
        jobs: usize,
        under: usize,
        rules: Vec<Line<'static>>,
        plan_rows: Vec<Line<'static>>,
    ) -> Rows {
        let jobs_least = if jobs == 0 {
            0
        } else {
            length(jobs.min(2) + 2)
        };
        let jobs_least = if left >= jobs_least { jobs_least } else { 0 };
        left -= jobs_least;
        let under = if under == 0 {
            0
        } else {
            length(under + 1).min(left)
        };
        left -= under;
        let mut plan = Vec::new();
        if !plan_rows.is_empty() && length(plan_rows.len() + 1) <= left {
            plan = plan_rows;
            if length(plan.len() + rules.len() + 1) <= left {
                plan.splice(0..0, rules);
            }
        }
        let plan_height = if plan.is_empty() {
            0
        } else {
            length(plan.len() + 1)
        };
        left -= plan_height;
        let jobs = if jobs_least == 0 {
            0
        } else {
            (jobs_least + left).min(length(jobs + 2))
        };
        Rows {
            plan,
            plan_height,
            under,
            jobs,
        }
    }
}

/// What the advice and auto sizing mean for `profile`, whole: its advice, what `a` would write or what auto sizing
/// last changed, and under auto sizing what its next look changes and why, and why a raise is refused.
fn meaning(
    sizing: &Sizing,
    profile: &ProfileSizing,
    width: usize,
    now: u64,
    app: &App,
    theme: Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            profile.name.clone(),
            theme
                .profile(app.profile_index(&profile.name))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            profile.advice_word(),
            theme.tone(profile.advice.tone().into()),
        ),
    ])];
    lines.extend(
        wrap(&sizing.advice_line(profile), width)
            .into_iter()
            .map(Line::from),
    );
    let last = sizing
        .changes
        .iter()
        .rfind(|change| change.profile == profile.name);
    let note = if let Some(change) = profile.advice.change().filter(|_| !sizing.auto) {
        Some(format!(
            "a writes memory = \"{}\", slots = {} into the config; the daemon checks it first",
            ByteSize(change.memory),
            change.slots
        ))
    } else {
        last.map(|change| {
            format!(
                "auto sizing {}: {}; runnerctl sizing undo {} writes it back",
                human::ago(change.at, now),
                change.line(),
                profile.name
            )
        })
    };
    if let Some(note) = note {
        lines.extend(
            wrap(&note, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.dim())),
        );
    }
    if let Some(plan) = sizing.auto_plan.as_ref().filter(|_| sizing.auto) {
        let (_, why) = human::plan_line(sizing, plan, profile, now);
        lines.push(Line::from(""));
        lines.extend(
            wrap(&why, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.dim())),
        );
        for refusal in plan.refusals(&profile.name) {
            lines.extend(
                wrap(&refusal, width)
                    .into_iter()
                    .map(|line| Line::styled(line, theme.dim())),
            );
        }
    }
    lines
}

/// The profile's memory peaks against its memory, a bar each: the median, p95, p99 and largest, under its memory and
/// slots, in its colour.
fn peaks(profile: &ProfileSizing, width: usize, app: &App, theme: Theme) -> Vec<Line<'static>> {
    let memory = &profile.peaks.memory;
    let rows = [
        ("P50", memory.p50),
        ("P95", memory.p95),
        ("P99", memory.p99),
        ("Max", memory.max),
    ];
    let label_width = rows
        .iter()
        .map(|(label, _)| label.width())
        .chain(["Memory".width()])
        .max()
        .unwrap_or(0);
    let figures: Vec<String> = rows.iter().map(|(_, value)| bytes(*value)).collect();
    let figure_width = figures
        .iter()
        .map(|figure| figure.width())
        .max()
        .unwrap_or(0);
    let bar = width.saturating_sub(label_width + 1 + figure_width + 1);
    let whole = profile.memory.max(memory.max).max(1);
    let style = theme.profile(app.profile_index(&profile.name));
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{:<label_width$} ", "Memory"), theme.dim()),
        Span::raw(format!("{} × {}", bytes(profile.memory), profile.slots)),
    ])];
    for ((label, value), figure) in rows.iter().zip(figures) {
        let filled = usize::from(scale(*value, whole, length(bar)));
        let cap = usize::from(scale(profile.memory, whole, length(bar)));
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<label_width$} "), theme.dim()),
            Span::styled("█".repeat(filled), style),
            Span::styled("░".repeat(cap.saturating_sub(filled)), theme.dim()),
            Span::raw(" ".repeat(bar - filled.max(cap) + 1)),
            Span::raw(format!("{figure:>figure_width$}")),
        ]));
    }
    lines
}

/// Under auto sizing, what its next look does to each profile, a row each; nothing while auto sizing is off, or from
/// a daemon that does not say what it does next.
fn plan_rows(
    sizing: &Sizing,
    width: usize,
    now: u64,
    app: &App,
    theme: Theme,
) -> Vec<Line<'static>> {
    let Some(plan) = sizing.auto_plan.as_ref().filter(|_| sizing.auto) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let name_width = sizing
        .profiles
        .iter()
        .map(|profile| profile.name.width())
        .max()
        .unwrap_or(0);
    for profile in &sizing.profiles {
        let (what, _) = human::plan_line(sizing, plan, profile, now);
        let room = width.saturating_sub(2 + name_width + 1);
        lines.push(Line::from(vec![
            Span::styled("■ ", theme.profile(app.profile_index(&profile.name))),
            Span::raw(format!("{:<name_width$} ", profile.name)),
            Span::raw(fit(&what, room).trim_end().to_owned()),
        ]));
    }
    lines
}

/// Under auto sizing, its rules and why no look changes anything if one would not, wrapped to `width`; nothing while
/// auto sizing is off, or from a daemon that does not say what it does next.
fn rules(sizing: &Sizing, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let Some(plan) = sizing.auto_plan.as_ref().filter(|_| sizing.auto) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
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
                .map(|line| Line::styled(line, theme.wait())),
        );
    }
    lines
}

/// A row per profile: its memory and slots, its jobs' peaks, core's figures (its jobs, how busy, how long waited,
/// warm starts, CPU p99), the advice.
fn profiles_table(app: &App, sizing: &Sizing, width: usize, theme: Theme) -> Table<'static> {
    let profiles = &sizing.profiles;
    let cells = |cell: &dyn Fn(&ProfileSizing) -> Span<'static>| -> Vec<Span<'static>> {
        profiles.iter().map(cell).collect()
    };
    let plain = |text: &dyn Fn(&ProfileSizing) -> String| -> Vec<Span<'static>> {
        profiles
            .iter()
            .map(|profile| Span::raw(text(profile)))
            .collect()
    };
    let mut columns = vec![
        TableColumn::names(
            "Profile",
            cells(&|profile| {
                Span::styled(
                    profile.name.clone(),
                    theme.profile(app.profile_index(&profile.name)),
                )
            }),
        ),
        TableColumn::figures(
            "Memory",
            plain(&|profile| format!("{} × {}", bytes(profile.memory), profile.slots)),
            0,
        ),
        TableColumn::figures("Runs", plain(&|profile| profile.peaks.jobs.to_string()), 10),
        TableColumn::figures("P50", plain(&|profile| bytes(profile.peaks.memory.p50)), 9),
        TableColumn::figures("P95", plain(&|profile| bytes(profile.peaks.memory.p95)), 0),
        TableColumn::figures("P99", plain(&|profile| bytes(profile.peaks.memory.p99)), 6),
        TableColumn::figures("Max", plain(&|profile| bytes(profile.peaks.memory.max)), 0),
        TableColumn::figures(
            "At cap",
            cells(&|profile| {
                if profile.peaks.at_cap > 0 {
                    Span::styled(profile.peaks.at_cap.to_string(), theme.tone(Tone::Bad))
                } else {
                    Span::raw("")
                }
            }),
            7,
        ),
        TableColumn::figures(
            "P50 warm/cold",
            plain(&|profile| {
                let starts = &profile.starts;
                if starts.warm > 0 {
                    format!(
                        "{} / {}",
                        duration(starts.warm_p50),
                        duration(starts.cold_p50)
                    )
                } else {
                    String::new()
                }
            }),
            5,
        ),
    ];
    columns.extend(figure_columns(profiles));
    columns.push(TableColumn::words(
        "Advice",
        cells(&|profile| {
            Span::styled(
                profile.advice_word(),
                theme.tone(profile.advice.tone().into()),
            )
        }),
        0,
    ));
    fitted_table(columns, width, None, theme)
}

/// Core's figures of each profile as columns, headed by their labels, each cell its value and what it is out of; the
/// warm starts give way first, then the jobs, how busy the slots were and the waits.
fn figure_columns(profiles: &[ProfileSizing]) -> Vec<TableColumn> {
    let figures: Vec<Vec<api::LabelledFigure>> =
        profiles.iter().map(ProfileSizing::figures).collect();
    let Some(first) = figures.first() else {
        return Vec::new();
    };
    first
        .iter()
        .enumerate()
        .map(|(at, head)| {
            let rank = match at {
                0 => 6,
                1 => 4,
                2 => 2,
                3 => 8,
                _ => 0,
            };
            let cells = figures
                .iter()
                .map(|row| {
                    Span::raw(row.get(at).map_or_else(String::new, |figure| {
                        figure.of.as_ref().map_or_else(
                            || figure.value.clone(),
                            |of| format!("{} {of}", figure.value),
                        )
                    }))
                })
                .collect();
            TableColumn::figures(&head.label, cells, rank)
        })
        .collect()
}

/// The CPU p99 of the runs at the profile's `cpus` now, `–` without one.
pub(super) fn cpu_cell(peaks: &api::Peaks) -> String {
    peaks.cpu_p99().map_or_else(|| "–".to_owned(), cores_used)
}

/// The selected profile's jobs, a row each: the repository and the job, cut last, then their runs and peaks, the
/// p99 giving way first, the wait and what each needs.
fn jobs_table(profile: &ProfileSizing, width: usize, theme: Theme) -> Table<'static> {
    let jobs = &profile.jobs;
    let plain = |text: &dyn Fn(&api::JobSizing) -> String| -> Vec<Span<'static>> {
        jobs.iter().map(|job| Span::raw(text(job))).collect()
    };
    let columns = vec![
        TableColumn::names("Repository", plain(&|job| job.repository.clone())),
        TableColumn::names(
            "Job",
            plain(&|job| job_title(job.workflow.as_deref(), &job.name, None).to_string()),
        ),
        TableColumn::figures("Runs", plain(&|job| job.peaks.jobs.to_string()), 2),
        TableColumn::figures("P95", plain(&|job| bytes(job.peaks.memory.p95)), 0),
        TableColumn::figures("P99", plain(&|job| bytes(job.peaks.memory.p99)), 3),
        TableColumn::figures("Max", plain(&|job| bytes(job.peaks.memory.max)), 0),
        TableColumn::figures("CPU p99", plain(&|job| cpu_cell(&job.peaks)), 1),
        TableColumn::figures("Wait p95", plain(&|job| duration(job.wait.p95)), 1),
        TableColumn::words(
            "Needs",
            jobs.iter()
                .map(|job| {
                    let needs = job.needs();
                    Span::styled(needs.text, theme.tone(needs.tone.into()))
                })
                .collect(),
            0,
        ),
    ];
    fitted_table(columns, width, None, theme)
}
