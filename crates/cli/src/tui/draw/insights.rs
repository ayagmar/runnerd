//! The insights view: what the job minutes would have cost on GitHub's runners, then what the history says, one row
//! per insight in its tone, and what the selected one means: whole, with the runs that show it, beside the list on a
//! wide terminal and under it on a narrow one. Under the list, the host's memory on the days CI ran and what jobs and
//! the drive wrote, each part a label beside its lines.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{self, job_title};
use unicode_width::UnicodeWidthStr;

use super::{Theme, beside, bold, fit, heading, length, picked, wrap};
use crate::human::ago;
use crate::tui::app::App;
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

/// What the host's memory section is called.
const HOST_TITLE: &str = "Host memory while CI ran";

pub(super) fn insights(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    now: u64,
    theme: Theme,
    hits: &mut Hits,
) {
    let name = api::View::Insights.title();
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let insights = match &app.insights {
        Reading::Read(insights) => insights,
        Reading::Unread(why) => {
            heading(frame, title, name, String::new(), theme);
            let text = match why {
                Unread::Waiting => "reading…".to_owned(),
                Unread::Unsupported => api::Feature::Insights.unsupported(),
                Unread::Failed(why) => why.clone(),
            };
            frame.render_widget(Line::styled(text, theme.dim()), body);
            return;
        }
    };
    heading(
        frame,
        title,
        name,
        insights.warning_line().map_or_else(
            || insights.window_line(),
            |line| format!("{line} {}", insights.window_line()),
        ),
        theme,
    );
    let (main, pane) = beside(frame, body, theme);
    let width = usize::from(main.width);
    // Whole or not at all: its basis and what it leaves out stay with the dollars.
    let savings: Vec<Line<'static>> = app
        .savings
        .as_ref()
        .map(|savings| wrap(&savings.month.headline(), width))
        .unwrap_or_default()
        .into_iter()
        .map(Line::from)
        .collect();
    let savings_height = if length(savings.len()) <= main.height / 4 {
        length(savings.len() + 1)
    } else {
        0
    };
    let rows = length(insights.insights.len().max(1));
    // On a narrow terminal the selected insight's meaning goes under the list.
    let under = if pane.is_none() {
        insights
            .insights
            .get(app.insight)
            .map(|insight| meaning(insights, insight, width, now, theme))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let rest = main
        .height
        .saturating_sub(savings_height + rows.min(main.height));
    let meaning_height = length(under.len() + 1).min(rest);
    let [savings_area, list_area, meaning_area, sections_area] = Layout::vertical([
        Constraint::Length(savings_height),
        Constraint::Length(rows),
        Constraint::Length(meaning_height),
        Constraint::Min(0),
    ])
    .areas(main);
    frame.render_widget(Paragraph::new(savings), savings_area);
    if insights.insights.is_empty() {
        frame.render_widget(Line::styled(insights.empty_line(), theme.dim()), list_area);
    } else {
        insight_rows(frame, list_area, app, insights, theme, hits);
    }
    let mut lines = vec![Line::from("")];
    lines.extend(under);
    frame.render_widget(Paragraph::new(lines), meaning_area);
    let sections = sections(insights, usize::from(sections_area.width), theme);
    let room = usize::from(sections_area.height);
    // Whole sections only, in order, a blank line before each.
    let mut shown: Vec<Line<'static>> = Vec::new();
    for section in sections {
        if shown.len() + 1 + section.len() > room {
            break;
        }
        shown.push(Line::from(""));
        shown.extend(section);
    }
    frame.render_widget(Paragraph::new(shown), sections_area);
    if let (Some(pane), Some(insight)) = (pane, insights.insights.get(app.insight)) {
        frame.render_widget(
            Paragraph::new(meaning(
                insights,
                insight,
                usize::from(pane.width),
                now,
                theme,
            )),
            pane,
        );
    }
}

/// What an insight means, whole: its kind in its tone, the job it is about, its sentence, the runs that show it,
/// newest first, which `Enter` opens, and its repository's waits this week against the week before.
fn meaning(
    insights: &api::Insights,
    insight: &api::Insight,
    width: usize,
    now: u64,
    theme: Theme,
) -> Vec<Line<'static>> {
    let tone = theme.of(insight.tone());
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", tone),
        Span::styled(insight.kind.label(), tone),
    ])];
    lines.push(Line::styled(
        fit(&insight.repository, width).trim_end().to_owned(),
        bold(),
    ));
    if let Some(job) = &insight.job {
        let title = job_title(insight.workflow.as_deref(), job, None).to_string();
        lines.push(Line::raw(fit(&title, width).trim_end().to_owned()));
    }
    lines.push(Line::from(""));
    lines.extend(wrap(&insight.sentence, width).into_iter().map(Line::from));
    if !insight.runs.is_empty() {
        lines.push(Line::from(""));
        let ages: Vec<String> = insight.runs.iter().map(|run| ago(run.at, now)).collect();
        let age_width = ages.iter().map(|age| age.width()).max().unwrap_or(0);
        for (run, age) in insight.runs.iter().zip(ages) {
            let attempt = if run.run_attempt > 1 {
                format!(" #{}", run.run_attempt)
            } else {
                String::new()
            };
            let branch = run.branch.as_deref().unwrap_or_default();
            let text = format!("run {}{attempt}  {branch}", run.run_id);
            lines.push(Line::from(vec![
                Span::styled(format!("{age:>age_width$}  "), theme.dim()),
                Span::raw(
                    fit(&text, width.saturating_sub(age_width + 2))
                        .trim_end()
                        .to_owned(),
                ),
            ]));
        }
        lines.push(Line::styled("enter opens the newest", theme.dim()));
    }
    if let Some(waits) = insights
        .waits
        .iter()
        .find(|waits| waits.repository == insight.repository)
    {
        lines.push(Line::from(""));
        lines.extend(waits_lines(waits, theme));
    }
    lines
}

/// A repository's waits this week, from queued to started, against the week before, as core words them.
fn waits_lines(waits: &api::RepositoryWaits, theme: Theme) -> Vec<Line<'static>> {
    let figures = waits.figures();
    let label_width = figures
        .iter()
        .map(|figure| figure.label.width())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::styled(api::RepositoryWaits::TITLE, bold())];
    lines.extend(figures.into_iter().map(|figure| {
        Line::from(vec![
            Span::styled(format!("{:<label_width$}  ", figure.label), theme.dim()),
            Span::raw(figure.value),
        ])
    }));
    lines
}

/// The host's memory on the days CI ran, and what jobs and the drive wrote: each a title, then each part's label
/// beside its lines, wrapped to `width`.
fn sections(insights: &api::Insights, width: usize, theme: Theme) -> Vec<Vec<Line<'static>>> {
    let mut sections = Vec::new();
    if !insights.host.is_empty() {
        let mut lines = vec![Line::styled(HOST_TITLE, bold())];
        lines.extend(
            insights
                .host
                .iter()
                .flat_map(|hazard| wrap(&hazard.dated_line(), width))
                .map(|line| Line::styled(line, theme.dim())),
        );
        sections.push(lines);
    }
    if let Some(disk) = &insights.disk {
        let parts = disk.sections();
        let label_width = parts
            .iter()
            .map(|part| part.title.width())
            .max()
            .unwrap_or(0);
        let room = width.saturating_sub(label_width + 2);
        let mut lines = vec![Line::styled(api::DiskWrites::TITLE, bold())];
        for part in parts {
            let wrapped: Vec<String> = part
                .lines
                .iter()
                .flat_map(|line| wrap(line, room))
                .collect();
            for (at, line) in wrapped.into_iter().enumerate() {
                let label = if at == 0 { part.title } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:<label_width$}  "), theme.dim()),
                    Span::raw(line),
                ]));
            }
        }
        sections.push(lines);
    }
    sections
}

/// A row per insight: a dot and its kind in its tone, then its sentence, cut; the selected one stays in sight.
fn insight_rows(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    insights: &api::Insights,
    theme: Theme,
    hits: &mut Hits,
) {
    let width = usize::from(area.width);
    let label_width = insights
        .insights
        .iter()
        .map(|insight| insight.kind.label().width())
        .max()
        .unwrap_or(0);
    let shown = usize::from(area.height);
    let first = app.insight.saturating_sub(shown.saturating_sub(1));
    let lines: Vec<Line<'_>> = insights
        .insights
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, insight)| {
            let rest = width.saturating_sub(2 + label_width + 2);
            let line = Line::from(vec![
                Span::styled("● ", theme.of(insight.tone())),
                Span::styled(
                    format!("{:<label_width$}  ", insight.kind.label()),
                    theme.of(insight.tone()),
                ),
                Span::raw(fit(&insight.sentence, rest)),
            ]);
            if at == app.insight {
                picked(line)
            } else {
                line
            }
        })
        .collect();
    hits.rows(area, List::Insights, first, insights.insights.len() - first);
    frame.render_widget(Paragraph::new(lines), area);
}
