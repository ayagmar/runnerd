//! What `runnerctl tui` draws: the overview (what needs attention, health, budget, a lane per slot, the queue,
//! repositories, latest jobs, the slice), the jobs (a table or a timeline), records, insights, sizing, caches, the
//! doctor and one job (timeline, steps on one time axis, memory and CPU, its failure, its folded log, its comparison).
//! Reads `App` and the time only, and returns where it drew what a click can reach; panes that do not fit a small
//! terminal are left out or drawn one line each. Body text is the terminal's own colour; dim is for secondary
//! metadata only, never a name. Colours follow the web dashboard: profiles in config order take its palette, mint
//! first and pink second; without colour, glyphs and bold carry the meaning.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use runnerd_core::api;
use runnerd_core::config::ByteSize;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Asked, Connection, JobView, Pane, Refused, Removal, View};
use super::mouse::{Hits, List, Target};
use super::overlay::{Overlay, Pick, Picker, Typing};
use super::prompts::Prompt;
use crate::human::Tone;
use caches::caches;
use history::history;
use insights::insights;
use job::{drop_least, job};
use overview::overview;
use records::records;
use settings::settings;
use sizing::sizing;

mod attention;
mod budget;
mod caches;
mod compare;
mod history;
mod insights;
mod job;
mod log;
mod overview;
mod records;
mod resources;
mod settings;
mod sizing;
mod slice;
mod slots;
mod steps;
mod timeline;

/// The web dashboard's profile colours as xterm-256 colours: mint, pink, lavender, yellow, blue, peach.
const PROFILES: [u8; 6] = [194, 224, 189, 229, 153, 223];

/// The dashboard's memory green, CPU lavender and waiting yellow.
const MEMORY: u8 = 114;

const CPU: u8 = 147;

const WAIT: u8 = 221;

/// How to style the screen: in colour, or with `NO_COLOR` in bold, dim and glyphs only.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    pub(crate) colour: bool,
}

impl Theme {
    fn fg(self, colour: Color) -> Style {
        if self.colour {
            Style::new().fg(colour)
        } else {
            Style::new()
        }
    }

    fn profile(self, index: Option<usize>) -> Style {
        index.map_or_else(
            || self.dim(),
            |index| self.fg(Color::Indexed(PROFILES[index % PROFILES.len()])),
        )
    }

    fn dim(self) -> Style {
        if self.colour {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    fn memory(self) -> Style {
        self.fg(Color::Indexed(MEMORY))
    }

    fn cpu(self) -> Style {
        self.fg(Color::Indexed(CPU))
    }

    fn wait(self) -> Style {
        self.fg(Color::Indexed(WAIT))
    }

    /// Core's tone of a fact as a style.
    pub(super) fn of(self, tone: api::Tone) -> Style {
        match tone {
            api::Tone::Warn => self.wait().add_modifier(Modifier::BOLD),
            tone => self.tone(tone.into()),
        }
    }

    pub(super) fn tone(self, tone: Tone) -> Style {
        match tone {
            Tone::Plain => Style::new(),
            Tone::Bold => Style::new().add_modifier(Modifier::BOLD),
            Tone::Dim => self.dim(),
            Tone::Good => self.memory(),
            Tone::Bad => self.fg(Color::LightRed).add_modifier(Modifier::BOLD),
            Tone::Busy => self.cpu(),
        }
    }
}

/// A column's heading, in capitals as the tables draw them.
fn column_title(column: api::Column) -> String {
    column.title().to_uppercase()
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// The whole screen, and where it drew what a click can reach.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, now: u64, theme: Theme) -> Hits {
    let mut hits = Hits::default();
    let [top, body, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    header(frame, top, app, theme, &mut hits);
    match app.view {
        View::Overview => overview(frame, body, app, now, theme, &mut hits),
        View::History => history(frame, body, app, now, theme, &mut hits),
        View::Records => records(frame, body, app, now, theme, &mut hits),
        View::Insights => insights(frame, body, app, theme, &mut hits),
        View::Sizing => sizing(frame, body, app, now, theme, &mut hits),
        View::Caches => caches(frame, body, app, now, theme, &mut hits),
        View::Settings => settings(frame, body, app, now, theme, &mut hits),
        View::Job => {
            if let Some(open) = &app.job {
                job(frame, body, app, open, now, theme, &mut hits);
            }
        }
    }
    footer(frame, bottom, app, theme);
    match &app.overlay {
        Overlay::Help => help(frame, body, &mut hits),
        Overlay::Pick(picker) => pick(frame, body, picker, theme, &mut hits),
        Overlay::None | Overlay::Input(_) => {}
    }
    if let Some(Prompt::Action(preview)) = app.prompts.top() {
        job::preview(frame, body, preview, theme, &mut hits);
    }
    hits
}

fn header(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme, hits: &mut Hits) {
    let mut spans = vec![Span::styled(" runnerd ", bold())];
    let version = Span::styled(format!("{}  ", app.live.status.version), theme.dim());
    let job = app
        .job
        .as_ref()
        .map(|open| Span::styled(format!(" job {} ", open.job.summary.id), selected()));
    let mut right = Vec::new();
    // What needs attention, counted, in the tone of its most urgent item; a click or `!` opens it.
    let attention = attention::count(app, theme);
    if let Some(count) = &attention {
        right.push(count.clone());
        right.push(Span::raw("  "));
    }
    if let Some(stopping) = app.live.status.stopping_line() {
        right.push(Span::styled(format!("{stopping}  "), theme.wait()));
    }
    match &app.connection {
        Connection::Live => right.push(Span::styled("■ live ", theme.memory())),
        Connection::Reconnecting(why) => right.push(Span::styled(
            format!("□ reconnecting: {why} "),
            theme.wait().add_modifier(Modifier::BOLD),
        )),
    }
    // GitHub's rate limit goes first when the tabs do not fit beside it.
    if let Some(limited) = app.live.status.github_line() {
        let limited = Span::styled(format!("{limited}  "), theme.wait());
        let least = " runnerd ".width()
            + Line::from(tabs_of(app, TabNames::Open)).width()
            + job.as_ref().map_or(0, Span::width);
        if least + limited.width() + Line::from(right.clone()).width() <= usize::from(area.width) {
            right.insert(usize::from(attention.is_some()) * 2, limited);
        }
    }
    let right = Line::from(right);
    // Where the tabs and what is on the right do not fit, the tabs not open show their digit only.
    let mut tabs = tabs_of(app, TabNames::Whole);
    let job_width = job.as_ref().map_or(0, Span::width);
    if " runnerd ".width() + Line::from(tabs.clone()).width() + job_width + right.width()
        > usize::from(area.width)
    {
        tabs = tabs_of(app, TabNames::Open);
    }
    spans.extend(tabs.iter().cloned());
    spans.extend(job);
    // The version goes first when the tabs and the connection do not fit beside it.
    let with_version = Line::from(spans.clone()).width() + version.width() + right.width()
        <= usize::from(area.width);
    if with_version {
        spans.insert(1, version.clone());
    }
    let mut x = area.x + length(" runnerd ".width());
    if with_version {
        x += length(version.width());
    }
    for (tab, span) in tabs.iter().enumerate() {
        let width = length(span.width());
        hits.add(
            Rect {
                x,
                width: width.min(area.right().saturating_sub(x)),
                ..area
            },
            Target::Tab(tab),
        );
        x += width;
    }
    let left = Line::from(spans);
    // What is on the right takes what the tabs leave, cut at its end.
    let right_width = length(right.width()).min(area.width.saturating_sub(length(left.width())));
    frame.render_widget(left, area);
    frame.render_widget(
        right.alignment(Alignment::Right),
        Rect {
            x: area.right().saturating_sub(right_width),
            width: right_width,
            ..area
        },
    );
    if let Some(count) = attention {
        hits.add(
            Rect {
                x: area.right().saturating_sub(right_width),
                width: length(count.width()).min(right_width),
                ..area
            },
            Target::Attention,
        );
    }
}

/// How much of a tab's name the header has room for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabNames {
    /// Every tab named.
    Whole,
    /// Only the open tab named; the others show their digit.
    Open,
}

/// The header's tabs, in core's order: the open one reversed, and a view of several sections names the one open.
fn tabs_of(app: &App, names: TabNames) -> Vec<Span<'static>> {
    (1..)
        .zip(View::tabs())
        .map(|(key, (shown, _))| {
            if app.view.group() == Some(shown) {
                let title = match app.view.section().filter(|_| View::of(shown).len() > 1) {
                    Some(section) => format!(" {key} {} · {} ", shown.title(), section.title()),
                    None => format!(" {key} {} ", shown.title()),
                };
                Span::styled(title, selected())
            } else if names == TabNames::Whole {
                Span::raw(format!(" {key} {} ", shown.title()))
            } else {
                Span::raw(format!(" {key} "))
            }
        })
        .collect()
}

fn footer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let prompt = app.prompts.top();
    let line = if let Overlay::Input(input) = &app.overlay {
        let ask = match input.typing {
            Typing::Jobs => " search names, workflows and branches: ",
            Typing::Log => " search the log's messages: ",
        };
        let room = usize::from(area.width).saturating_sub(ask.width() + 1);
        // The end of what is typed stays in sight.
        let text: String = {
            let mut shown: Vec<char> = Vec::new();
            for character in input.text.chars().rev() {
                shown.push(character);
                if shown.iter().collect::<String>().width() > room {
                    shown.pop();
                    break;
                }
            }
            shown.into_iter().rev().collect()
        };
        Line::from(vec![
            Span::styled(ask, bold()),
            Span::raw(text),
            Span::styled("█", theme.dim()),
        ])
    } else if let Some(Prompt::Force(refused)) = prompt {
        force_line(refused, usize::from(area.width), theme)
    } else if let Some(Prompt::Remove(removal)) = prompt {
        let ask = match removal {
            Removal::Job(job) => {
                format!(" y removes the record of job {job}, any other key keeps it")
            }
            Removal::All => " y removes every record, any other key keeps them".to_owned(),
        };
        Line::styled(ask, bold())
    } else if let Some(Prompt::Resize(resizing)) = prompt {
        Line::styled(
            format!(
                " y writes memory = \"{}\", slots = {} for {} into the daemon's config, any other key keeps it",
                ByteSize(resizing.resize.memory),
                resizing.resize.slots,
                resizing.profile
            ),
            bold(),
        )
    } else if let Some(Prompt::Empty(volume)) = prompt {
        Line::styled(
            format!(" y empties {volume}, any other key keeps it"),
            bold(),
        )
    } else if let Some(Prompt::Action(preview)) = prompt {
        Line::styled(
            if preview.confirm.is_some() {
                format!(" y: {}, any other key sends nothing", preview.confirm_label)
            } else {
                " any key closes; nothing can be sent".to_owned()
            },
            bold(),
        )
    } else if let Some(asked) = app.job.as_ref().and_then(JobView::asking) {
        Line::styled(
            match asked {
                Asked::Preview(_) => " reading what it would do…",
                Asked::Send(_) => " sending…",
            },
            theme.dim(),
        )
    } else if let Some(flash) = &app.flash {
        let style = match flash.tone {
            api::Tone::Neutral => Style::new(),
            tone => theme.of(tone),
        };
        Line::styled(format!(" {}", flash.text), style)
    } else {
        Line::styled(
            format!(
                " {}",
                footer_keys(app, usize::from(area.width).saturating_sub(1))
            ),
            theme.dim(),
        )
    };
    frame.render_widget(line, area);
}

/// The question a refused pause asks: whether to cancel the runs that hold it, and what they are.
fn force_line(refused: &Refused, width: usize, theme: Theme) -> Line<'static> {
    let ask = if refused.jobs == 1 {
        " y cancels the job's run and pauses, any other key keeps it running: ".to_owned()
    } else {
        // Jobs of one workflow share a run, so the count is of jobs.
        format!(
            " y cancels the runs of {} jobs and pauses, any other key keeps them running: ",
            refused.jobs
        )
    };
    // Narrow, the question shortens so what it cancels still shows: the start of the message names it.
    let ask = if ask.width() + refused.message.width().min(40) <= width {
        ask
    } else if refused.jobs == 1 {
        " y cancels its run and pauses: ".to_owned()
    } else {
        format!(" y cancels {} jobs' runs and pauses: ", refused.jobs)
    };
    let rest = width.saturating_sub(ask.width());
    Line::from(vec![
        Span::styled(ask, bold()),
        Span::styled(fit(&refused.message, rest), theme.wait()),
    ])
}

/// The keys that matter on the view, those that do not fit left out from the least used: each view's own first,
/// then moving and opening, then help and quit, which stay.
fn footer_keys(app: &App, width: usize) -> String {
    let views = format!("1-{} views", View::tabs().len());
    let mut keys: Vec<(u8, &str)> = match app.view {
        View::Overview => {
            let mut keys = vec![
                (1, "tab pane"),
                (2, "j/k move"),
                (1, "enter open"),
                (1, "p/r pause/resume"),
                (3, "P/U all"),
            ];
            if app.pane == Pane::Slots {
                keys.push((2, "R reseed"));
            }
            if !app.notices().is_empty() && app.pane != Pane::Attention {
                keys.insert(0, (1, "! attention"));
            }
            keys
        }
        View::History => vec![
            (1, "/ search"),
            (1, "f filter"),
            (2, "s sort"),
            (2, "t time"),
            (2, "l timeline"),
            (3, "x clear"),
            (2, "enter open"),
            (3, "] [ page"),
        ],
        View::Records => vec![
            (2, "j/k move"),
            (2, "enter open"),
            (1, "d remove"),
            (1, "D remove all"),
        ],
        View::Insights => vec![(2, "j/k move")],
        View::Sizing => vec![(2, "j/k move"), (1, "a apply the advice")],
        View::Caches => vec![(2, "j/k move"), (1, "d empty")],
        View::Settings => vec![(2, "j/k move"), (1, "u read again")],
        View::Job => {
            let mut keys = vec![
                (1, "enter fold"),
                (1, "/ search"),
                (2, "n/N match"),
                (1, "c compare"),
            ];
            if app.live.status.job_actions.offered() {
                keys.push((2, "x cancel"));
                keys.push((2, "a/A re-run"));
            }
            keys.extend([(3, "G follow"), (1, "esc back")]);
            keys
        }
    };
    keys.extend([(4, views.as_str()), (0, "? help"), (0, "q quit")]);
    let line = |keys: &[(u8, &str)]| {
        keys.iter()
            .map(|(_, key)| *key)
            .collect::<Vec<_>>()
            .join("  ")
    };
    drop_least(&mut keys, 1, |keys| line(keys).width() <= width);
    line(&keys)
}

/// The help row for the number keys: the digits, and the views they open, in the order of the tabs.
pub(super) fn view_keys(tabs: &[(api::View, View)]) -> (String, String) {
    let digits: Vec<String> = (1..=tabs.len()).map(|key| key.to_string()).collect();
    let names: Vec<String> = tabs
        .iter()
        .map(|(shown, _)| shown.title().to_lowercase())
        .collect();
    (digits.join(" "), names.join(", "))
}

/// The rows of a table drawn in `table` under its header, from `offset`, as `list`'s targets.
fn table_hits(hits: &mut Hits, table: Rect, list: List, offset: usize, count: usize) {
    let [_, rows] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(table);
    hits.rows(rows, list, offset, count.saturating_sub(offset));
}

/// A box of `width` and `height` in the middle of `area`, at most as large.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn help(frame: &mut Frame<'_>, area: Rect, hits: &mut Hits) {
    let (digits, views) = view_keys(&View::tabs());
    // At most 20, so it fits a terminal of 24 rows.
    let keys: [(&str, &str); 20] = [
        (digits.as_str(), views.as_str()),
        ("digit again", "the view's next section, such as records"),
        ("tab", "overview: the next pane"),
        ("j k ↓ ↑", "move, or scroll the log; PgDn PgUp by ten"),
        ("enter esc", "open; back"),
        ("!", "what needs attention"),
        ("/ n N", "search; the next and the previous match"),
        ("f s t", "jobs: filter, sort, time range"),
        ("l x", "jobs: timeline; clear the filters"),
        ("] [", "jobs: the next and the previous page"),
        ("enter", "job: fold or unfold the step or group"),
        ("G g", "follow the log, or go to its start"),
        ("c", "job: compare with an earlier run"),
        ("x a A", "job: cancel, re-run it, re-run the failed"),
        ("p r P U", "pause or resume the repository, or all"),
        ("R", "reseed the profile's snapshot"),
        ("d D", "remove a record, or all; empty a cache"),
        ("a u", "sizing: write the advice; settings: read again"),
        ("click wheel", "select, again to open; scroll"),
        ("? ctrl-z q", "this help; suspend; quit"),
    ];
    let popup = centred(area, 62, length(keys.len()) + 2);
    let lines: Vec<Line<'_>> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<12}"), bold()),
                Span::raw(*what),
            ])
        })
        .collect();
    hits.cover(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        popup,
    );
}

/// The list open over the view, the option the keys are on reversed; in the list of filters, what each is set to.
fn pick(frame: &mut Frame<'_>, area: Rect, picker: &Picker, theme: Theme, hits: &mut Hits) {
    let column = |column: api::Column| format!(" {} ", column.title());
    let title = match picker.pick {
        Pick::Filters => " Filter ".to_owned(),
        Pick::Order => " Sort ".to_owned(),
        Pick::Range => column(api::Column::Queued),
        Pick::Outcome => column(api::Column::Outcome),
        Pick::Repository => column(api::Column::Repository),
        Pick::Workflow => column(api::Column::Workflow),
        Pick::Branch => column(api::Column::Branch),
        Pick::Record => column(api::Column::Record),
    };
    let choices = picker.choices();
    let label_width = choices
        .iter()
        .map(|choice| choice.label.width())
        .max()
        .unwrap_or(0);
    let note_width = choices
        .iter()
        .map(|choice| choice.note.width())
        .max()
        .unwrap_or(0);
    let wanted = label_width + if note_width > 0 { note_width + 2 } else { 0 } + 4;
    let popup = centred(area, length(wanted.max(24)), length(choices.len()) + 2);
    let inner = Block::bordered().inner(popup);
    let shown = usize::from(inner.height);
    // The option the keys are on stays in sight.
    let first = picker.at().saturating_sub(shown.saturating_sub(1));
    let width = usize::from(inner.width).saturating_sub(2);
    let lines: Vec<Line<'_>> = choices
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(at, choice)| {
            let line = if choice.note.is_empty() {
                Line::from(format!(" {}", fit(&choice.label, width)))
            } else {
                let room = width.saturating_sub(label_width + 2);
                Line::from(vec![
                    Span::raw(format!(" {:<label_width$}  ", choice.label)),
                    Span::styled(fit(&choice.note, room), theme.dim()),
                ])
            };
            if at == picker.at() {
                line.style(selected())
            } else {
                line
            }
        })
        .collect();
    hits.cover(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        popup,
    );
    for (offset, at) in (first..choices.len()).take(shown).enumerate() {
        hits.add(
            Rect {
                y: inner.y + length(offset),
                height: 1,
                ..inner
            },
            Target::Choice(at),
        );
    }
}

/// A section's title on the left and a note on the right, on one line.
/// The note is cut to leave the title and two cells beside it.
fn heading(frame: &mut Frame<'_>, area: Rect, title: &str, note: String, theme: Theme) {
    styled_heading(frame, area, title, note, theme.dim());
}

/// `heading` with its note in `style`.
fn styled_heading(frame: &mut Frame<'_>, area: Rect, title: &str, note: String, style: Style) {
    frame.render_widget(Line::styled(title.to_owned(), bold()), area);
    let room = usize::from(area.width).saturating_sub(title.width() + 2);
    let note = if note.width() <= room {
        note
    } else {
        fit(&note, room).trim_end().to_owned()
    };
    frame.render_widget(Line::styled(note, style).alignment(Alignment::Right), area);
}

/// A count of rows or cells as a height or width.
fn length(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

/// A sparkline of the last `width` values as one line of eighth blocks.
fn spark(values: impl DoubleEndedIterator<Item = u64>, max: u64, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut bars: Vec<char> = values
        .rev()
        .take(width)
        .map(|value| {
            let level = u128::from(value) * 7 / u128::from(max.max(1));
            BARS[usize::try_from(level).unwrap_or(7).min(7)]
        })
        .collect();
    bars.reverse();
    bars.into_iter().collect()
}

/// `text` in lines of at most `width` cells, broken between words; a word longer than a line is cut by [`fit`].
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.width() + 1 + word.width() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(fit(word, width).trim_end().to_owned()),
        }
    }
    lines
}

/// `text` cut or padded to `width` terminal cells, ending in `…` when cut.
///
/// The cut is measured on the whole prefix, not per character: a sequence such as `❤️` (a character and a
/// variation selector) takes more cells than its characters do one by one.
fn fit(text: &str, width: usize) -> String {
    let mut cut = String::new();
    if text.width() <= width {
        cut.push_str(text);
    } else if width > 0 {
        for character in text.chars() {
            cut.push(character);
            if cut.width() + 1 > width {
                cut.pop();
                break;
            }
        }
        cut.push('…');
    }
    let used = cut.width();
    cut.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    cut
}

fn list_block(title: &str, focused: bool, theme: Theme) -> Block<'_> {
    Block::new()
        .borders(Borders::TOP)
        .border_style(if focused { bold() } else { theme.dim() })
        .title(Span::styled(
            format!(" {title} "),
            if focused { selected() } else { bold() },
        ))
}

/// A table's column widths: 0 fills what the others leave.
fn widths(columns: &[(&str, usize)]) -> Vec<Constraint> {
    columns
        .iter()
        .map(|(_, width)| match width {
            0 => Constraint::Fill(1),
            width => Constraint::Length(u16::try_from(*width).unwrap_or(u16::MAX)),
        })
        .collect()
}
