//! The insights view: what the job minutes would have cost on GitHub's runners, then what the history says, one row
//! per insight in its tone, led by the figure that changed, and what the selected one means: whole, with the runs
//! that show it, beside the list on a wide terminal and under it on a narrow one. Under the list, the host's memory on
//! the days CI ran, what jobs and the drive wrote and why each profile's jobs queued, as core's figures, the drive's shares as one bar and the
//! writers that wrote most as ranked bars; as core's sentences where the width does not hold them.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use runnerd_core::api::{self, job_title};
use unicode_width::UnicodeWidthStr;

use super::budget::scale;
use super::{Theme, beside, bold, clip, figure_rows, fit, heading, length, picked, wrap};
use crate::human::{ago, bytes};
use crate::tui::app::App;
use crate::tui::mouse::{Hits, List};
use crate::tui::reading::{Reading, Unread};

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
    heading(frame, title, name, insights.heading().to_string(), theme);
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
    // The list, then the selected insight's meaning, then the savings where they still fit whole: the savings give
    // way before the meaning.
    let rest = main.height.saturating_sub(rows.min(main.height));
    let meaning_height = if under.is_empty() {
        0
    } else {
        length(under.len() + 1).min(rest)
    };
    let rest = rest - meaning_height;
    let savings_height =
        if length(savings.len()) <= main.height / 4 && length(savings.len() + 1) <= rest {
            length(savings.len() + 1)
        } else {
            0
        };
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
    lines.extend(clip(
        under,
        usize::from(meaning_height.saturating_sub(1)),
        theme,
    ));
    frame.render_widget(Paragraph::new(lines), meaning_area);
    let sections = sections(insights, usize::from(sections_area.width), theme);
    // In order, a blank line before each; what does not fit is counted.
    let mut shown: Vec<Line<'static>> = Vec::new();
    for section in sections {
        shown.push(Line::from(""));
        shown.extend(section);
    }
    frame.render_widget(
        Paragraph::new(clip(shown, usize::from(sections_area.height), theme)),
        sections_area,
    );
    if let (Some(pane), Some(insight)) = (pane, insights.insights.get(app.insight)) {
        let lines = meaning(insights, insight, usize::from(pane.width), now, theme);
        frame.render_widget(
            Paragraph::new(clip(lines, usize::from(pane.height), theme)),
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
    if let Some(figure) = insight.figure() {
        lines.extend(figure_line(&figure, width, theme));
    }
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
        lines.extend(waits_lines(waits, width, theme));
    }
    lines
}

/// A repository's waits this week, from queued to started, against the week before, as core words them: its title,
/// then its figures side by side in `width`.
fn waits_lines(waits: &api::RepositoryWaits, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(api::RepositoryWaits::TITLE, bold())];
    lines.extend(figure_rows(
        waits.figures().iter().map(Into::into),
        width,
        theme,
    ));
    lines
}

/// The host's memory on the days CI ran, what jobs and the drive wrote, and why each profile's jobs queued: each a
/// title, then what it says in `width`.
fn sections(insights: &api::Insights, width: usize, theme: Theme) -> Vec<Vec<Line<'static>>> {
    let mut sections = Vec::new();
    if !insights.host.is_empty() {
        let mut lines = vec![Line::styled(api::HostHazard::TITLE, bold())];
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
        let mut lines = vec![Line::styled(api::DiskWrites::TITLE, bold())];
        if width >= DISK_FIGURES_FROM {
            lines.extend(disk_figures(disk, width, theme));
        } else {
            lines.extend(disk_sentences(disk, width, theme));
        }
        sections.push(lines);
    }
    if !insights.queue.is_empty() {
        let mut lines = vec![Line::styled(api::ProfileQueue::TITLE, bold())];
        for profile in &insights.queue {
            lines.extend(wrap(&profile.line(), width).into_iter().map(Line::raw));
            lines.extend(figure_rows(
                profile.figures().iter().map(Into::into),
                width,
                theme,
            ));
        }
        sections.push(lines);
    }
    sections
}

/// The fewest cells the disk writes are drawn in as figures and bars; narrower, they are core's sentences.
const DISK_FIGURES_FROM: usize = 40;

/// The disk writes as figures: what the jobs and the drive wrote, dim under them what the figures leave out, the
/// drive's shares as one bar with its key, and the
/// workflows and jobs that wrote most as ranked bars with core's note.
fn disk_figures(disk: &api::DiskWrites, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = figure_rows(disk.figures().iter().map(Into::into), width, theme);
    lines.extend(
        disk.caveats()
            .iter()
            .flat_map(|caveat| wrap(caveat, width))
            .map(|line| Line::styled(line, theme.dim())),
    );
    if let Some(drive) = disk.drive.as_ref().filter(|drive| !drive.shares.is_empty()) {
        lines.push(Line::from(""));
        lines.push(Line::raw(api::DiskWrites::SHARES_TITLE));
        lines.extend(shares(&drive.shares.parts(), width, theme));
    }
    for (title, tops) in [
        (api::DiskWrites::WORKFLOWS_TITLE, &disk.workflows),
        (api::DiskWrites::TOP_JOBS_TITLE, &disk.top_jobs),
    ] {
        if !tops.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::raw(title));
            lines.extend(ranked(tops, width, theme));
        }
    }
    lines
}

/// The disk writes as core's sentences, each part's title beside its lines, wrapped to `width`.
fn disk_sentences(disk: &api::DiskWrites, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let parts = disk.sections();
    let label_width = parts
        .iter()
        .map(|part| part.title.width())
        .max()
        .unwrap_or(0);
    let room = width.saturating_sub(label_width + 2);
    let mut lines = Vec::new();
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
    lines
}

/// A share's glyph and style, the dashboard's colour of its owner: a shape each, so the bar reads without colour.
fn owner_mark(owner: api::ShareOwner, theme: Theme) -> (&'static str, Style) {
    match owner {
        api::ShareOwner::Jobs => ("█", theme.profile(Some(0))),
        api::ShareOwner::Slice => ("▓", theme.profile(Some(2))),
        api::ShareOwner::Daemon => ("▒", theme.profile(Some(3))),
        api::ShareOwner::Unknown => ("░", theme.dim()),
    }
}

/// The drive's shares as one bar `width` cells long, each part with bytes at least a cell and as long as its bytes, then its key: each part's glyph,
/// percent and owner, side by side where they fit.
fn shares(parts: &[api::DriveShare], width: usize, theme: Theme) -> Vec<Line<'static>> {
    // A cell each for every part with bytes, so each in the key is in the bar; the rest as long as their bytes.
    let owned = parts.iter().filter(|part| part.bytes > 0).count();
    let room = u128::try_from(width.saturating_sub(owned)).unwrap_or(0);
    let whole: u128 = parts.iter().map(|part| u128::from(part.bytes)).sum();
    let mut bar: Vec<Span<'static>> = Vec::new();
    let mut counted: u128 = 0;
    let mut drawn: u128 = 0;
    for part in parts.iter().filter(|part| part.bytes > 0) {
        counted += u128::from(part.bytes);
        let end = (counted * room + whole / 2) / whole.max(1);
        let cells = usize::try_from(end - drawn).unwrap_or(0) + 1;
        drawn = end;
        let (glyph, style) = owner_mark(part.owner, theme);
        bar.push(Span::styled(glyph.repeat(cells), style));
    }
    let mut lines = vec![Line::from(bar)];
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for part in parts {
        let (glyph, style) = owner_mark(part.owner, theme);
        let percent = format!("{}%", part.percent);
        let text = format!(" {}", part.owner.label());
        let adds = glyph.width() + 1 + percent.width() + text.width();
        if used > 0 && used + 3 + adds > width {
            lines.push(Line::from(std::mem::take(&mut line)));
            used = 0;
        }
        if used > 0 {
            line.push(Span::raw("   "));
            used += 3;
        }
        line.extend([
            Span::styled(format!("{glyph} "), style),
            Span::styled(percent, bold()),
            Span::raw(
                fit(&text, width.saturating_sub(used + adds - text.width()))
                    .trim_end()
                    .to_owned(),
            ),
        ]);
        used += adds;
    }
    if !line.is_empty() {
        lines.push(Line::from(line));
    }
    lines
}

/// The cells of a ranked bar.
const RANKED_BAR: usize = 10;

/// The workflows or jobs that wrote most, a row each: a bar as long as its share of the first's bytes, at least a cell, its bytes,
/// its name and core's note. Where a row does not hold all of it, each takes two: its name, then the bar, bytes and
/// note.
fn ranked(tops: &[api::WriteTop], width: usize, theme: Theme) -> Vec<Line<'static>> {
    let most = tops.iter().map(|top| top.total).max().unwrap_or(0);
    let values: Vec<String> = tops.iter().map(|top| bytes(top.total)).collect();
    let value_width = values.iter().map(|value| value.width()).max().unwrap_or(0);
    let head = RANKED_BAR + 1 + value_width + 2;
    let one_row = tops
        .iter()
        .all(|top| head + top.name().width() + 2 + top.note().width() <= width);
    let mut lines = Vec::new();
    for (top, value) in tops.iter().zip(values) {
        // A writer with bytes shows a cell, however far behind the first.
        let filled = usize::from(scale(top.total, most.max(1), length(RANKED_BAR)))
            .max(usize::from(top.total > 0));
        let mut spans = vec![
            Span::styled("█".repeat(filled), theme.memory()),
            Span::styled("░".repeat(RANKED_BAR - filled), theme.dim()),
            Span::styled(format!(" {value:>value_width$}  "), bold()),
        ];
        let note = top.note();
        if one_row {
            spans.push(Span::raw(top.name()));
            spans.push(Span::styled(format!("  {note}"), theme.dim()));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::raw(fit(&top.name(), width).trim_end().to_owned()));
            spans.push(Span::styled(
                fit(&note, width.saturating_sub(head)).trim_end().to_owned(),
                theme.dim(),
            ));
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// An insight's figure, the number that changed, in bold before its dim label; the label wraps under it where the
/// line is too short.
fn figure_line(figure: &api::InsightFigure, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let value = fit(&figure.value, width).trim_end().to_owned();
    if value.width() + 1 + figure.label.width() <= width {
        return vec![Line::from(vec![
            Span::styled(value, bold()),
            Span::styled(format!(" {}", figure.label), theme.dim()),
        ])];
    }
    let mut lines = vec![Line::styled(value, bold())];
    lines.extend(
        wrap(&figure.label, width)
            .into_iter()
            .map(|line| Line::styled(line, theme.dim())),
    );
    lines
}

/// The fewest cells an insight row keeps for its sentence before its figure takes any.
const SENTENCE_LEAST: usize = 12;

/// A row per insight: a dot and its kind in its tone, its figure where the sentence keeps [`SENTENCE_LEAST`] cells,
/// then its sentence, cut; the selected one stays in sight.
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
    let figures: Vec<String> = insights
        .insights
        .iter()
        .map(|insight| {
            insight
                .figure()
                .map(|figure| figure.value)
                .unwrap_or_default()
        })
        .collect();
    let figure_width = figures
        .iter()
        .map(|figure| figure.width())
        .max()
        .unwrap_or(0);
    // The sentence keeps its room before the figure takes any: narrow, the figure is left to the meaning.
    let lead = 2 + label_width + 2;
    let figure_room = if figure_width > 0 && lead + figure_width + 2 + SENTENCE_LEAST <= width {
        figure_width + 2
    } else {
        0
    };
    let shown = usize::from(area.height);
    let first = app.insight.saturating_sub(shown.saturating_sub(1));
    let lines: Vec<Line<'_>> = insights
        .insights
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, insight)| {
            let rest = width.saturating_sub(lead + figure_room);
            let line = Line::from(vec![
                Span::styled("● ", theme.of(insight.tone())),
                Span::styled(
                    format!("{:<label_width$}  ", insight.kind.label()),
                    theme.of(insight.tone()),
                ),
                Span::styled(
                    if figure_room > 0 {
                        format!("{:>figure_width$}  ", figures[at])
                    } else {
                        String::new()
                    },
                    bold(),
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
