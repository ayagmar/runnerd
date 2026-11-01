//! What `runnerctl tui` draws: the overview (budget, slots, the slice, repositories, latest jobs), the history, the
//! records, sizing and one job (timeline, steps on one time axis, memory and CPU, its log). Reads `App` and the time
//! only; panes that do not fit a small terminal are left out or drawn one line each. Colours follow the web dashboard:
//! profiles in config order take its palette, mint first and pink second; without colour, glyphs and bold carry the
//! meaning.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use runnerd_core::config::ByteSize;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Connection, Outcome, Pane, Removal, View};
use crate::human::Tone;
use history::history;
use job::{drop_least, job};
use overview::overview;
use records::records;
use sizing::sizing;

mod budget;
mod history;
mod job;
mod log;
mod overview;
mod records;
mod resources;
mod sizing;
mod slice;
mod slots;
mod steps;

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

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// The whole screen.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, now: u64, theme: Theme) {
    let [top, body, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    header(frame, top, app, theme);
    match app.view {
        View::Overview => overview(frame, body, app, now, theme),
        View::History => history(frame, body, app, now, theme),
        View::Records => records(frame, body, app, now, theme),
        View::Sizing => sizing(frame, body, app, now, theme),
        View::Job => {
            if let Some(open) = &app.job {
                job(frame, body, app, open, now, theme);
            }
        }
    }
    footer(frame, bottom, app, theme);
    if app.help {
        help(frame, body);
    }
}

fn header(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let mut spans = vec![Span::styled(" runnerd ", bold())];
    let version = Span::styled(format!("{}  ", app.live.status.version), theme.dim());
    for (key, name, view) in [
        ("1", "Overview", View::Overview),
        ("2", "History", View::History),
        ("3", "Records", View::Records),
        ("4", "Sizing", View::Sizing),
    ] {
        let style = if app.view == view {
            selected()
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!(" {key} {name} "), style));
    }
    if let Some(open) = &app.job {
        spans.push(Span::styled(
            format!(" job {} ", open.job.summary.id),
            selected(),
        ));
    }
    let mut right = Vec::new();
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
    let right = Line::from(right);
    // The version goes first when the tabs and the connection do not fit beside it.
    if Line::from(spans.clone()).width() + version.width() + right.width()
        <= usize::from(area.width)
    {
        spans.insert(1, version);
    }
    frame.render_widget(Line::from(spans), area);
    frame.render_widget(right.alignment(Alignment::Right), area);
}

fn footer(frame: &mut Frame<'_>, area: Rect, app: &App, theme: Theme) {
    let line = if let Some(refused) = &app.confirm {
        let ask = if refused.jobs == 1 {
            " y cancels the job's run and pauses, any other key keeps it running: ".to_owned()
        } else {
            // Jobs of one workflow share a run, so the count is of jobs.
            format!(
                " y cancels the runs of {} jobs and pauses, any other key keeps them running: ",
                refused.jobs
            )
        };
        let rest = usize::from(area.width).saturating_sub(ask.width());
        Line::from(vec![
            Span::styled(ask, bold()),
            Span::styled(fit(&refused.message, rest), theme.wait()),
        ])
    } else if let Some(removal) = app.removal {
        let ask = match removal {
            Removal::Job(job) => {
                format!(" y removes the record of job {job}, any other key keeps it")
            }
            Removal::All => " y removes every record, any other key keeps them".to_owned(),
        };
        Line::styled(ask, bold())
    } else if let Some(resizing) = &app.resize {
        Line::styled(
            format!(
                " y writes memory = \"{}\", slots = {} for {} into the daemon's config, any other key keeps it",
                ByteSize(resizing.resize.memory),
                resizing.resize.slots,
                resizing.profile
            ),
            bold(),
        )
    } else if let Some(flash) = &app.flash {
        let style = if flash.outcome == Outcome::Failed {
            theme.tone(Tone::Bad)
        } else {
            theme.memory()
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

/// The keys that matter on the view, those that do not fit left out from the least used: each view's own first,
/// then moving and opening, then help and quit, which stay.
fn footer_keys(app: &App, width: usize) -> String {
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
            keys
        }
        View::History => vec![
            (2, "j/k move"),
            (2, "enter open"),
            (1, "c outcome"),
            (1, "s repository"),
            (1, "w workflow"),
            (1, "] [ page"),
        ],
        View::Records => vec![
            (2, "j/k move"),
            (2, "enter open"),
            (1, "d remove"),
            (1, "D remove all"),
        ],
        View::Sizing => vec![(2, "j/k move"), (1, "a apply the advice")],
        View::Job => vec![
            (2, "j/k scroll"),
            (2, "G follow"),
            (1, "p/r pause/resume"),
            (3, "R reseed"),
            (1, "esc back"),
        ],
    };
    keys.extend([(4, "1-4 views"), (0, "? help"), (0, "q quit")]);
    let line = |keys: &[(u8, &str)]| {
        keys.iter()
            .map(|(_, key)| *key)
            .collect::<Vec<_>>()
            .join("  ")
    };
    drop_least(&mut keys, 1, |keys| line(keys).width() <= width);
    line(&keys)
}

fn help(frame: &mut Frame<'_>, area: Rect) {
    // At most 20, so it fits a terminal of 24 rows.
    const KEYS: [(&str, &str); 16] = [
        ("1 2 3 4", "overview, history, records, sizing; o and h too"),
        ("tab", "overview: slots, waiting, repositories, latest"),
        ("j k ↓ ↑", "move, or scroll the log"),
        ("PgDn PgUp", "move by ten"),
        ("enter esc", "open the job; back from it"),
        ("G g", "follow the log, or go to its start"),
        ("p r", "pause the repository, or resume it"),
        ("P U", "pause every repository, or resume them"),
        ("R", "reseed the profile's snapshot"),
        ("c s w", "history: cycle outcome, repository, workflow"),
        ("] [", "history: next and previous page"),
        ("d D", "records: remove the record, or every record"),
        ("a", "sizing: write the advice into the config"),
        ("?", "this help; any key closes it"),
        ("ctrl-z", "suspend; the shell's fg brings it back"),
        ("q ctrl-c", "quit"),
    ];
    let width = area.width.min(60);
    let height = area
        .height
        .min(u16::try_from(KEYS.len()).unwrap_or(u16::MAX) + 2);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let lines: Vec<Line<'_>> = KEYS
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<10}"), bold()),
                Span::raw(*what),
            ])
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys ")),
        popup,
    );
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
