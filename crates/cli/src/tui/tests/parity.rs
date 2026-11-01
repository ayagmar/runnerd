//! The TUI's frames of the shared parity fixtures state core's facts in core's words, and colour each job's outcome
//! by core's meaning, as the dashboard's and `runnerctl`'s tests check theirs (ADR 0062). Facts, not layout: each is
//! found on its row of a frame wide enough to show it whole, not at a cell. The TUI writes outcomes in capitals.

use std::collections::BTreeMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use runnerd_core::api::{BuildCacheCounts, Column, JobPage, JobState, View};
use runnerd_fixtures::{Daemon, Facts, JobRow, NOW, TIMED_OUT, daemon, facts};

use super::super::app::{App, Effect, Msg, View as AppView};
use super::super::draw::{Theme, draw, view_keys};
use super::key;
use crate::{human, watch};

const COLOUR: Theme = Theme { colour: true };

/// The overview of the fixtures: their status, running jobs and latest finished ones.
fn app(daemon: &Daemon) -> App {
    let running: BTreeMap<u64, _> = daemon
        .jobs
        .iter()
        .filter(|job| job.summary.state == JobState::Running)
        .map(|job| (job.summary.id, job.clone()))
        .collect();
    let live = watch::State {
        status: daemon.status.clone(),
        running,
        samples: BTreeMap::new(),
        host: None,
        recent: daemon.finished(),
    };
    App::new(live, None, Vec::new(), Vec::new())
}

/// A frame wide enough that nothing is fitted, as text rows and as its cells.
fn frame(app: &App) -> (Vec<String>, Buffer) {
    frame_of(app, 300, 60)
}

/// A frame `width` cells wide and `height` rows high.
fn frame_of(app: &App, width: u16, height: u16) -> (Vec<String>, Buffer) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| draw(frame, app, NOW, COLOUR))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    (rows, buffer)
}

fn shows(rows: &[String], text: &str) -> bool {
    rows.iter().any(|row| row.contains(text))
}

/// Each finished job has a row with its outcome, in the colour of its meaning, and its name, wait and run time.
fn assert_rows(rows: &[String], buffer: &Buffer, facts: &[JobRow]) {
    for fact in facts {
        let badge = fact.label.to_uppercase();
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| {
                row.contains(&badge)
                    && row.contains(&fact.title)
                    && [&fact.waited, &fact.ran]
                        .into_iter()
                        .flatten()
                        .all(|figure| row.contains(figure.as_str()))
            })
            .unwrap_or_else(|| panic!("no row says {fact:?}:\n{}", rows.join("\n")));
        let x = row[..row.find(&badge).unwrap()].chars().count();
        let style = buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())].style();
        let expected = COLOUR.tone(human::outcome_tone(fact.kind));
        assert_eq!(style.fg, expected.fg, "{badge} is {:?}: {row}", fact.kind);
    }
}

/// A slot's state as a sentence starts, as the slot cards write it.
fn sentence(line: &str) -> String {
    let mut chars = line.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The history, answered with the fixtures' finished jobs.
fn history(app: &mut App, daemon: &Daemon) {
    let generation = app
        .update(key(KeyCode::Char('2')))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::History { generation, .. } => Some(generation),
            _ => None,
        })
        .unwrap();
    app.update(Msg::History {
        generation,
        page: Ok(JobPage {
            jobs: daemon.finished(),
            next: None,
        }),
    });
}

fn fixtures() -> (Daemon, Facts) {
    let daemon = daemon();
    let facts = facts(&daemon);
    (daemon, facts)
}

#[test]
fn the_overview_states_the_budget_health_slots_waits_and_latest_jobs_as_core_does() {
    let (daemon, facts) = fixtures();
    let (rows, buffer) = frame(&app(&daemon));
    let all = rows.join("\n");
    assert!(shows(&rows, &facts.budget), "{all}");
    assert!(shows(&rows, &facts.cpu), "{all}");
    assert!(shows(&rows, &facts.host), "{all}");
    assert!(shows(&rows, facts.stopping), "{}: {all}", facts.stopping);
    for figure in &facts.figures {
        let line = format!("{}: {}", figure.label, figure.value);
        assert!(shows(&rows, &line), "{line}: {all}");
        if let Some(of) = &figure.of {
            assert!(shows(&rows, of), "{of}: {all}");
        }
    }
    // A running slot shows its job instead.
    for slot in facts.slots.iter().filter(|slot| slot.word != "running") {
        assert!(shows(&rows, &sentence(&slot.line)), "{}: {all}", slot.line);
    }
    for wait in &facts.waits {
        assert!(shows(&rows, &wait.reason), "{}: {all}", wait.reason);
    }
    assert_rows(&rows, &buffer, &facts.rows);
}

#[test]
fn the_history_states_each_outcome_wait_and_run_time_as_core_does() {
    let (daemon, facts) = fixtures();
    let mut app = app(&daemon);
    history(&mut app, &daemon);
    let (rows, buffer) = frame(&app);
    assert_rows(&rows, &buffer, &facts.rows);
}

#[test]
fn a_job_states_its_outcome_steps_caps_build_cache_and_failed_runs_as_core_does() {
    let (daemon, facts) = fixtures();
    let mut app = app(&daemon);
    history(&mut app, &daemon);
    let at = daemon
        .finished()
        .iter()
        .position(|job| job.id == TIMED_OUT)
        .unwrap();
    for _ in 0..at {
        app.update(key(KeyCode::Char('j')));
    }
    assert!(
        app.update(key(KeyCode::Enter))
            .contains(&Effect::OpenJob(TIMED_OUT)),
        "Enter on the timed-out row opens its page"
    );
    app.update(Msg::Job(daemon.job(TIMED_OUT).unwrap().clone()));
    let mut runs = daemon.runs(TIMED_OUT);
    // Newest first, as the daemon lists them.
    runs.reverse();
    app.update(Msg::Runs {
        job: TIMED_OUT,
        page: Ok(JobPage {
            jobs: runs,
            next: None,
        }),
    });
    let (rows, _) = frame(&app);
    let all = rows.join("\n");
    let job = &facts.job;
    assert!(
        shows(
            &rows,
            &format!("{}  {}", job.row.title, job.row.label.to_uppercase())
        ),
        "{all}"
    );
    for figure in [
        &job.limits,
        &job.cpu_time,
        &job.unseen,
        &job.cpu_held,
        &job.build_cache_refused,
        &format!("{} of {} failed", job.failed, job.finished),
    ] {
        assert!(shows(&rows, figure), "{figure}: {all}");
    }
    for total in &job.totals {
        let figure = format!("{} {}", total.label, total.value);
        assert!(shows(&rows, &figure), "{figure}: {all}");
    }
    for step in &job.steps {
        let row = rows
            .iter()
            .find(|row| row.trim_start().starts_with(&step.name))
            .unwrap_or_else(|| panic!("no row for {}: {all}", step.name));
        // A step's row ends in how long it took, `–` without its end.
        let took = step.took.as_deref().unwrap_or("–");
        assert!(row.trim_end().ends_with(took), "{took}: {row}");
    }
    // What held its CPU back stays whole on a common terminal, where the details' line drops what does not fit.
    let (rows, _) = frame_of(&app, 120, 40);
    assert!(shows(&rows, &job.cpu_held), "{}", rows.join("\n"));
}

/// The columns `names` in order on the first row of `rows` that has them all.
fn assert_heads(rows: &[String], names: &[Column]) {
    let heads: Vec<String> = names
        .iter()
        .map(|column| column.title().to_uppercase())
        .collect();
    let row = rows
        .iter()
        .find(|row| row.contains(&heads[0]))
        .unwrap_or_else(|| panic!("no row with {}: {rows:?}", heads[0]));
    let mut at = 0;
    for head in &heads {
        let found = row[at..]
            .find(head)
            .unwrap_or_else(|| panic!("{head} after {at}: {row}"));
        at += found + head.len();
    }
}

#[test]
fn the_tabs_follow_core_s_views_and_each_digit_opens_its_tab() {
    use ratatui::style::Modifier;
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    // The views core lists that the TUI draws, by name, in core's order.
    let (rows, _) = frame(&app);
    let shown: Vec<&str> = View::ALL
        .iter()
        .map(|view| view.title())
        .filter(|title| rows[0].contains(&format!(" {title} ")))
        .collect();
    assert_eq!(shown.first(), Some(&"Overview"), "{}", rows[0]);
    assert!(
        shown.contains(&"Records") && shown.contains(&"Sizing"),
        "{}",
        rows[0]
    );
    for (number, title) in (1..).zip(&shown) {
        let digit = char::from_digit(number, 10).unwrap();
        app.update(key(KeyCode::Char(digit)));
        let (rows, buffer) = frame(&app);
        let tab = format!(" {digit} {title} ");
        let x = rows[0]
            .find(&tab)
            .unwrap_or_else(|| panic!("{tab} is a tab, in core's order: {}", rows[0]));
        assert!(
            buffer[(u16::try_from(x + 3).unwrap(), 0)]
                .modifier
                .contains(Modifier::REVERSED),
            "key {digit} opens {title}: {}",
            rows[0]
        );
    }
    // A different order of core's views is a different order of tabs and of keys.
    let swapped = [View::Records, View::Overview, View::Config, View::Sizing];
    let tabs: Vec<_> = AppView::tabs_of(&swapped)
        .into_iter()
        .map(|(_, tui)| tui)
        .collect();
    assert_eq!(tabs, [AppView::Records, AppView::Overview, AppView::Sizing]);
}

#[test]
fn the_footer_and_the_help_name_the_views_the_keys_open() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    let tabs = AppView::tabs();
    let (rows, _) = frame(&app);
    let views = format!("1-{} views", tabs.len());
    assert!(shows(&rows, &views), "the footer says {views}: {rows:?}");
    app.update(key(KeyCode::Char('?')));
    let (rows, _) = frame(&app);
    let (digits, names) = view_keys(&tabs);
    let row = rows
        .iter()
        .find(|row| row.contains(&digits))
        .unwrap_or_else(|| panic!("help lists {digits}: {rows:?}"));
    assert!(row.contains(&names), "help names the views {names}: {row}");
    // Keys follow the order of the tabs, whatever it is, and the names follow the keys.
    let swapped = AppView::tabs_of(&[View::Records, View::Overview, View::Config, View::Sizing]);
    assert_eq!(
        view_keys(&swapped),
        (
            "1 2 3".to_owned(),
            "records, overview, sizing; o and h too".to_owned()
        )
    );
}

#[test]
fn the_history_and_records_tables_are_headed_as_core_names_their_columns() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    history(&mut app, &daemon);
    let (rows, _) = frame(&app);
    assert_heads(
        &rows,
        &[
            Column::Outcome,
            Column::Repository,
            Column::Job,
            Column::Branch,
            Column::Waited,
            Column::Ran,
            Column::MemoryPeak,
            Column::Cpu,
            Column::Queued,
        ],
    );
    app.update(key(KeyCode::Char('3')));
    app.update(Msg::Records(daemon.records.clone()));
    let (rows, _) = frame(&app);
    assert_heads(
        &rows,
        &[
            Column::Id,
            Column::Repository,
            Column::Job,
            Column::Size,
            Column::Made,
        ],
    );
}

#[test]
fn a_job_with_cache_hits_and_refused_uploads_shows_both() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    history(&mut app, &daemon);
    let at = daemon
        .finished()
        .iter()
        .position(|job| job.id == TIMED_OUT)
        .unwrap();
    for _ in 0..at {
        app.update(key(KeyCode::Char('j')));
    }
    app.update(key(KeyCode::Enter));
    let mut job = daemon.job(TIMED_OUT).unwrap().clone();
    job.summary.metrics.build_cache = BuildCacheCounts {
        hits: 3,
        misses: 1,
        read_bytes: 3 << 29,
        written_bytes: 1 << 30,
        refused: 3,
        writer: true,
    };
    let line = job
        .summary
        .metrics
        .build_cache
        .activity()
        .unwrap()
        .short_line();
    app.update(Msg::Job(job));
    let (rows, _) = frame(&app);
    assert!(
        shows(&rows, &line) && line.contains("3 uploads refused"),
        "{line}: {}",
        rows.join("\n")
    );
}

#[test]
fn records_near_max_total_say_so_as_core_does() {
    let (daemon, facts) = fixtures();
    let mut app = app(&daemon);
    app.update(key(KeyCode::Char('3')));
    app.update(Msg::Records(daemon.records.clone()));
    let (rows, _) = frame(&app);
    let all = rows.join("\n");
    assert!(shows(&rows, &facts.records_pressure), "{all}");
    assert!(shows(&rows, &facts.records_usage), "{all}");
}

#[test]
fn the_overview_says_what_the_months_job_minutes_would_have_cost_as_core_words_it() {
    let (daemon, facts) = fixtures();
    let mut app = app(&daemon);
    app.update(Msg::Savings(Box::new(daemon.savings.clone())));
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    for width in [80, 100, 120, 300] {
        let (rows, _) = frame_of(&app, width, 60);
        // Wrapped, never cut: its basis and what it leaves out stay with the dollars.
        assert!(
            words(&rows.join(" ")).contains(&words(&facts.savings)),
            "{width} columns:\n{}",
            rows.join("\n")
        );
    }
    let (rows, _) = frame_of(&app, 80, 12);
    assert!(
        !rows.iter().any(|row| row.contains('$')),
        "too short a terminal leaves the figure out rather than cut it:\n{}",
        rows.join("\n")
    );
}

#[test]
fn sizing_states_what_blocks_auto_sizing_and_what_it_does_next_as_core_does() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    app.update(key(KeyCode::Char('4')));
    let sizing = runnerd_fixtures::sizing(NOW);
    let plan = sizing.auto_plan.clone().unwrap();
    app.update(Msg::Sizing(sizing.clone()));
    let (rows, _) = frame_of(&app, 120, 40);
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let shown = words(&rows.join(" "));
    // Wrapped, never cut.
    assert!(
        shown.contains(&words(&plan.blocked_line().unwrap())),
        "{}",
        rows.join("\n")
    );
    let refusals = plan.refusals("node");
    assert_eq!(
        refusals.len(),
        2,
        "a CPU and a slot raise refused: {refusals:?}"
    );
    for refusal in &refusals {
        assert!(shown.contains(&words(refusal)), "{}", rows.join("\n"));
    }
    // A profile `[host] memory_ceiling` holds reads as such, not as one whose slice has to grow.
    let android = sizing
        .profiles
        .iter()
        .find(|p| p.name == "android")
        .unwrap();
    assert_eq!(android.advice_word(), "at ceiling");
    assert!(shown.contains("at ceiling"), "{}", rows.join("\n"));
    // And one its `max_memory` holds.
    let gradle = sizing.profiles.iter().find(|p| p.name == "gradle").unwrap();
    assert_eq!(gradle.advice_word(), "at max_memory");
    assert!(shown.contains("at max_memory"), "{}", rows.join("\n"));
}

#[test]
fn the_health_line_counts_the_warning_insights_as_core_does() {
    let mut app = app(&daemon());
    let insights = runnerd_fixtures::insights(NOW);
    let warnings = insights.warning_count();
    app.update(Msg::Insights(Box::new(insights)));
    let (rows, _) = frame(&app);
    assert!(
        shows(&rows, &format!("{warnings} insights: runnerctl insights")),
        "{}",
        rows.join("\n")
    );
}
