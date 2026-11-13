//! The TUI's frames of the shared parity fixtures state core's facts in core's words, and colour each job's outcome
//! by core's meaning, as the dashboard's and `runnerctl`'s tests check theirs (ADR 0062). Facts, not layout: each is
//! found on its row of a frame wide enough to show it whole, not at a cell. The TUI writes outcomes in capitals.

use std::collections::BTreeMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use runnerd_core::api::{BuildCacheCounts, Column, JobAction, JobPage, JobState, Section, View};
use runnerd_fixtures::{Daemon, Facts, JobRow, NOW, TIMED_OUT, daemon, facts};

use super::super::app::{App, Effect, Msg, View as AppView};
use super::super::draw::{Theme, draw, view_keys};
use super::super::prompts::Prompt;
use super::{key, listed};
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
        .draw(|frame| {
            draw(frame, app, NOW, COLOUR);
        })
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
        list: Ok(listed(JobPage {
            jobs: daemon.finished(),
            next: None,
        })),
    });
}

fn fixtures() -> (Daemon, Facts) {
    let daemon = daemon();
    let facts = facts(&daemon);
    (daemon, facts)
}

#[test]
fn a_host_limit_past_memory_max_reads_as_core_says_it() {
    let daemon = runnerd_fixtures::past_memory_max();
    let facts = facts(&daemon);
    let (rows, _) = frame(&app(&daemon));
    assert!(shows(&rows, &facts.host), "{}", rows.join("\n"));
}

#[test]
fn a_host_credit_and_the_growth_a_held_job_waits_for_read_as_core_says_them() {
    let daemon = runnerd_fixtures::host_credit();
    let facts = facts(&daemon);
    let (rows, _) = frame(&app(&daemon));
    let all = rows.join("\n");
    assert!(shows(&rows, &facts.host), "{}: {all}", facts.host);
    for wait in &facts.waits {
        assert!(shows(&rows, &wait.reason), "{}: {all}", wait.reason);
    }
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
    assert!(shows(&rows, facts.github), "{}: {all}", facts.github);
    for figure in &facts.figures {
        let line = format!("{}: {}", figure.label, figure.value);
        assert!(shows(&rows, &line), "{line}: {all}");
        if let Some(of) = &figure.of {
            assert!(shows(&rows, of), "{of}: {all}");
        }
    }
    // A running slot shows its job instead.
    for slot in facts.slots.iter().filter(|slot| slot.word != "running") {
        assert!(shows(&rows, &slot.line), "{}: {all}", slot.line);
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
        list: Ok(listed(JobPage {
            jobs: runs,
            next: None,
        })),
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
        shown.contains(&"Jobs") && shown.contains(&"Capacity"),
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
    let swapped = [View::Jobs, View::Overview, View::Settings, View::Capacity];
    let tabs: Vec<_> = AppView::tabs_of(&swapped)
        .into_iter()
        .map(|(_, tui)| tui)
        .collect();
    assert_eq!(
        tabs,
        [
            AppView::History,
            AppView::Overview,
            AppView::Settings,
            AppView::Sizing
        ]
    );
}

#[test]
fn a_views_digit_again_opens_its_next_section_and_the_tab_names_it() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    let jobs = AppView::tabs()
        .iter()
        .position(|(view, _)| *view == View::Jobs)
        .unwrap();
    let digit = char::from_digit(u32::try_from(jobs + 1).unwrap(), 10).unwrap();
    let mut opened = Vec::new();
    for _ in 0..3 {
        app.update(key(KeyCode::Char(digit)));
        opened.push(app.view);
    }
    assert_eq!(
        opened,
        [AppView::History, AppView::Records, AppView::History],
        "each section core lists for Jobs, in its order, then round again"
    );
    app.update(key(KeyCode::Char(digit)));
    let (rows, _) = frame(&app);
    let tab = format!(
        " {digit} {} · {} ",
        View::Jobs.title(),
        Section::Records.title()
    );
    assert!(rows[0].contains(&tab), "{tab}: {}", rows[0]);
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
    let swapped = AppView::tabs_of(&[View::Jobs, View::Overview, View::Settings, View::Capacity]);
    assert_eq!(
        view_keys(&swapped),
        (
            "1 2 3 4".to_owned(),
            "jobs, overview, settings, capacity".to_owned()
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
    // From the history, the Jobs key again opens its next section.
    app.update(key(KeyCode::Char('2')));
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
    super::open_records(&mut app);
    app.update(Msg::Records(daemon.records.clone()));
    let (rows, _) = frame(&app);
    let all = rows.join("\n");
    assert!(shows(&rows, &facts.records_pressure), "{all}");
    assert!(shows(&rows, &facts.records_usage), "{all}");
}

#[test]
fn the_insights_view_says_what_the_months_job_minutes_would_have_cost_as_core_words_it() {
    let (daemon, facts) = fixtures();
    let mut app = app(&daemon);
    app.update(Msg::Savings(Box::new(daemon.savings.clone())));
    app.update(Msg::Insights(Ok(Box::new(runnerd_fixtures::insights(NOW)))));
    let (rows, _) = frame(&app);
    assert!(
        !rows.iter().any(|row| row.contains('$')),
        "the overview leaves the savings to the insights view:\n{}",
        rows.join("\n")
    );
    app.update(super::tab(View::Insights));
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
    let (rows, _) = frame_of(&app, 80, 9);
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
    app.update(super::tab(View::Capacity));
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
fn the_insights_view_states_each_insight_and_the_host_as_core_does() {
    let mut app = app(&daemon());
    let insights = runnerd_fixtures::insights(NOW);
    app.update(Msg::Insights(Ok(Box::new(insights.clone()))));
    app.update(super::tab(View::Insights));
    let (rows, buffer) = frame(&app);
    let all = rows.join("\n");
    if let Some(warning) = insights.warning_line() {
        assert!(shows(&rows, &warning), "{warning}: {all}");
    }
    assert!(shows(&rows, &insights.window_line()), "{all}");
    for insight in &insights.insights {
        let label = insight.kind.label();
        let row = rows
            .iter()
            .position(|row| row.contains(label) && row.contains(&insight.sentence[..20]))
            .unwrap_or_else(|| panic!("{label}: {all}"));
        let x = rows[row].find("● ").unwrap();
        let x = u16::try_from(rows[row][..x].chars().count()).unwrap();
        let y = u16::try_from(row).unwrap();
        assert_eq!(
            Some(buffer[(x, y)].fg),
            COLOUR
                .of(insight.tone())
                .fg
                .or(Some(ratatui::style::Color::Reset)),
            "{label} in its tone: {all}"
        );
    }
    for hazard in &insights.host {
        assert!(shows(&rows, &hazard.dated_line()), "{all}");
    }
}

/// Opens `job`'s page from the jobs view, as a person would, and reads it.
fn open(app: &mut App, job: &runnerd_core::api::Job) {
    app.update(key(KeyCode::Char('o')));
    let generation = app
        .update(super::tab(View::Jobs))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::History { generation, .. } => Some(generation),
            _ => None,
        })
        .unwrap();
    app.update(Msg::History {
        generation,
        list: Ok(listed(JobPage {
            jobs: vec![job.summary.clone()],
            next: None,
        })),
    });
    assert!(
        app.update(key(KeyCode::Enter))
            .contains(&Effect::OpenJob(job.summary.id)),
        "Enter opens job {}",
        job.summary.id
    );
    app.update(Msg::Job(job.clone()));
}

/// The cell under the first `text` in `rows`.
fn cell_of<'buffer>(
    rows: &[String],
    buffer: &'buffer Buffer,
    text: &str,
) -> &'buffer ratatui::buffer::Cell {
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.contains(text))
        .unwrap_or_else(|| panic!("{text}:\n{}", rows.join("\n")));
    let x = row[..row.find(text).unwrap()].chars().count();
    &buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())]
}

#[test]
fn the_overview_lists_what_needs_attention_in_core_s_words_and_tones() {
    let daemon = daemon();
    let attention = runnerd_fixtures::attention(&daemon, &runnerd_fixtures::doctor());
    let mut app = app(&daemon);
    app.update(Msg::Attention(Ok(attention.clone())));
    let (rows, buffer) = frame(&app);
    let all = rows.join("\n");
    // The overview shows as many as its spare rows allow, at least five, and counts the rest as core does.
    let shown = rows.iter().filter(|row| row.starts_with("▲ ")).count();
    assert!(shown >= 5.min(attention.items.len()), "{all}");
    let facts = runnerd_fixtures::attention_facts(&attention, shown);
    for fact in facts.items.iter().take(shown) {
        // A title longer than the row ends in an ellipsis; its insight or check has it whole.
        let start: String = fact.title.chars().take(120).collect();
        let row = rows
            .iter()
            .find(|row| row.contains(&start))
            .unwrap_or_else(|| panic!("{}: {all}", fact.title));
        let mark = format!("▲ {start}");
        if let Some(fix) = &fact.fix {
            assert!(
                row.contains(fix) || shows(&rows, fix),
                "{fix}, what clears it: {all}"
            );
        }
        if row.contains(&mark) {
            assert_eq!(
                Some(cell_of(&rows, &buffer, &mark).fg),
                COLOUR
                    .of(fact.tone)
                    .fg
                    .or(Some(ratatui::style::Color::Reset)),
                "{} in its tone",
                fact.title
            );
        }
    }
    if let Some(more) = &facts.more {
        assert!(shows(&rows, more), "{more}: {all}");
    }
    // The header counts them, and the count opens the list.
    assert!(
        rows[0].contains(&attention.items.len().to_string()),
        "{}",
        rows[0]
    );
}

/// The words of `text`, without a popup's or a list's borders, so a wrapped sentence reads as one.
fn words(text: &str) -> String {
    text.split_whitespace()
        .filter(|word| *word != "│")
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_selected_attention_item_shows_under_the_list_while_it_is_focused() {
    let daemon = daemon();
    let attention = runnerd_fixtures::attention(&daemon, &runnerd_fixtures::doctor());
    let mut app = app(&daemon);
    app.update(Msg::Attention(Ok(attention.clone())));
    app.update(key(KeyCode::Char('!')));
    for (at, item) in attention.items.iter().enumerate() {
        for (width, height) in [(80, 24), (120, 32)] {
            let (rows, _) = frame_of(&app, width, height);
            let start: String = item.title.chars().take(10).collect();
            assert!(
                shows(&rows, &format!("▲ {start}")),
                "item {at}'s row stays in the list at {width}x{height}:\n{}",
                rows.join("\n")
            );
            let shown = words(&rows.join(" "));
            let whole = shown.contains(&words(&item.title))
                && item
                    .fix
                    .as_ref()
                    .is_none_or(|fix| shown.contains(&words(&fix.line())));
            // A short terminal cuts a long item's sentence, and says so.
            assert!(
                whole || (width < 120 && rows.iter().any(|row| row.trim_end().ends_with(" …"))),
                "item {at}'s title and fix whole, or marked cut, at {width}x{height}:\n{}",
                rows.join("\n")
            );
        }
        app.update(key(KeyCode::Char('j')));
    }
}

#[test]
fn the_overview_says_how_many_repositories_and_latest_jobs_do_not_fit() {
    let daemon = daemon();
    let app = app(&daemon);
    for (width, height) in [(60, 20), (80, 24), (120, 32)] {
        let (rows, _) = frame_of(&app, width, height);
        let all = rows.join("\n");
        for (heading, names) in [
            (
                "Repositories",
                daemon
                    .status
                    .repositories
                    .iter()
                    .map(|repository| format!("■ {}", repository.name))
                    .collect::<Vec<_>>(),
            ),
            (
                "Latest jobs",
                daemon
                    .finished()
                    .iter()
                    .map(|job| job.outcome().label.to_uppercase())
                    .collect(),
            ),
        ] {
            let Some(row) = rows
                .iter()
                .position(|row| row.contains(&format!(" {heading} ")))
            else {
                continue;
            };
            let listed = rows[row + 1..]
                .iter()
                .take_while(|row| !row.contains(" Latest jobs ") && !row.contains(" Slice "))
                .filter(|row| names.iter().any(|name| row.contains(name.as_str())))
                .count();
            if listed < names.len() {
                let hidden = names.len() - listed;
                assert!(
                    rows[row].contains(&format!("{hidden} more below")),
                    "{heading}: {hidden} not shown at {width}x{height}:\n{all}"
                );
            }
        }
    }
}

#[test]
fn an_older_daemon_without_the_attention_route_reads_as_not_supported_never_as_empty() {
    let mut app = app(&daemon());
    app.update(Msg::Attention(Err(
        super::super::reading::Unread::Unsupported,
    )));
    let (rows, _) = frame(&app);
    assert!(
        shows(&rows, runnerd_core::api::NOT_SUPPORTED),
        "{}",
        rows.join("\n")
    );
}

#[test]
fn a_job_s_log_states_its_evidence_and_its_failure_first_as_core_does() {
    let daemon = daemon();
    let facts = runnerd_fixtures::log_facts();
    for logged in runnerd_fixtures::logged_jobs() {
        let id = logged.job.summary.id;
        let fact = facts.iter().find(|fact| fact.job == id).unwrap();
        let mut app = app(&daemon);
        open(&mut app, &logged.job);
        app.update(Msg::Outline {
            job: id,
            outline: Ok(logged.outline.clone()),
        });
        app.update(Msg::Lines {
            job: id,
            lines: fact.lines.clone(),
        });
        let (rows, _) = frame_of(&app, 200, 60);
        let all = rows.join("\n");
        if let Some(evidence) = fact.evidence {
            assert!(shows(&rows, evidence), "job {id}: {evidence}: {all}");
        }
        if let Some(headline) = &fact.headline {
            let at = rows.iter().position(|row| row.contains(headline.as_str()));
            let log = rows.iter().position(|row| row.contains(" Log "));
            assert!(
                at.is_some() && at < log,
                "job {id}: {headline}, above the log: {all}"
            );
        }
    }
}

#[test]
fn the_jobs_view_marks_each_record_and_its_timeline_says_each_end_as_core_does() {
    let lens = runnerd_fixtures::lens_facts();
    let history = runnerd_fixtures::lens_history();
    let mut app = app(&daemon());
    let generation = app
        .update(super::tab(View::Jobs))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::History { generation, .. } => Some(generation),
            _ => None,
        })
        .unwrap();
    app.update(Msg::History {
        generation,
        list: Ok(listed(JobPage {
            jobs: history.jobs.iter().map(|job| job.summary.clone()).collect(),
            next: None,
        })),
    });
    let (rows, buffer) = frame(&app);
    for mark in &lens.marks {
        let row = rows
            .iter()
            .position(|row| row.contains(mark.words))
            .unwrap_or_else(|| panic!("{}: {}", mark.words, rows.join("\n")));
        let x = rows[row][..rows[row].find(mark.words).unwrap()]
            .chars()
            .count();
        assert_eq!(
            Some(buffer[(u16::try_from(x).unwrap(), u16::try_from(row).unwrap())].fg),
            COLOUR
                .of(mark.tone)
                .fg
                .or(Some(ratatui::style::Color::Reset)),
            "{} in its tone",
            mark.words
        );
    }
    let generation = app
        .update(key(KeyCode::Char('l')))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Timeline { generation, .. } => Some(generation),
            _ => None,
        })
        .expect("the timeline lens reads the timeline");
    app.update(Msg::Timeline {
        generation,
        timeline: Ok(runnerd_fixtures::lens_timeline()),
    });
    let (rows, _) = frame(&app);
    let all = rows.join("\n");
    for end in &lens.ends {
        assert!(
            shows(&rows, end.words),
            "job {}: {}: {all}",
            end.job,
            end.words
        );
    }
    for gap in &lens.gaps {
        assert!(shows(&rows, gap.words), "{}: {all}", gap.words);
    }
}

#[test]
fn a_job_beside_its_baseline_states_each_figure_and_step_as_core_does() {
    let lens = runnerd_fixtures::lens_facts();
    let history = runnerd_fixtures::lens_history();
    let job = |id| {
        history
            .jobs
            .iter()
            .find(|job| job.summary.id == id)
            .unwrap()
    };
    let mut app = app(&daemon());
    open(&mut app, job(runnerd_fixtures::LENS_COMPARED));
    let id = runnerd_fixtures::LENS_COMPARED;
    assert!(
        app.update(key(KeyCode::Char('c')))
            .contains(&Effect::Compare(id)),
        "c asks for the comparison"
    );
    app.update(Msg::Compared {
        job: id,
        comparison: Ok(runnerd_fixtures::lens_comparison(id, None).unwrap()),
    });
    let (rows, _) = frame_of(&app, 200, 80);
    let all = rows.join("\n");
    // A row each: the label, both sides, and the change or why the step is not paired.
    for delta in &lens.compared {
        assert!(
            rows.iter().any(|row| {
                row.contains(&delta.label)
                    && row.contains(&delta.this)
                    && row.contains(&delta.baseline)
                    && row.contains(delta.matched.unwrap_or(delta.change.as_str()))
            }),
            "{delta:?}: {all}"
        );
    }

    let first = runnerd_fixtures::LENS_FIRST_RUN;
    open(&mut app, job(first));
    app.update(key(KeyCode::Char('c')));
    app.update(Msg::Compared {
        job: first,
        comparison: Ok(runnerd_fixtures::lens_comparison(first, None).unwrap()),
    });
    let (rows, _) = frame_of(&app, 200, 80);
    assert!(shows(&rows, &lens.first_run), "{}", rows.join("\n"));
}

#[test]
fn cancel_and_re_run_show_core_s_preview_and_send_only_on_y() {
    let daemon = daemon();
    let actions = runnerd_fixtures::job_actions(&daemon);
    let mut app = app(&daemon);
    open(&mut app, daemon.job(TIMED_OUT).unwrap());
    let asked = app.update(key(KeyCode::Char('x')));
    assert!(
        matches!(asked.as_slice(), [Effect::Act { query, .. }] if !query.force && query.confirm.is_none()),
        "x asks for the preview, sending nothing: {asked:?}"
    );
    for preview in &actions.previews {
        app.update(key(KeyCode::Char(action_key(preview.action))));
        app.update(Msg::Preview(Box::new(preview.clone())));
        let (rows, _) = frame_of(&app, 200, 60);
        // The popup's border is not part of what it says.
        let words = |text: &str| {
            text.split_whitespace()
                .filter(|word| *word != "│")
                .collect::<Vec<_>>()
                .join(" ")
        };
        let shown = words(&rows.join(" "));
        assert!(
            shown.contains(&words(&preview.title)),
            "{}",
            rows.join("\n")
        );
        assert!(
            shown.contains(&words(&preview.scope)),
            "{}",
            rows.join("\n")
        );
        for note in &preview.notes {
            assert!(shown.contains(&words(note)), "{note}: {}", rows.join("\n"));
        }
        let answered = app.update(key(KeyCode::Char('y')));
        match &preview.confirm {
            Some(confirm) => assert!(
                matches!(answered.as_slice(), [Effect::Act { query, action, .. }]
                    if query.force && query.confirm.as_ref() == Some(confirm) && *action == preview.action),
                "y sends the preview's confirmation: {answered:?}"
            ),
            None => assert!(answered.is_empty(), "nothing to send: {answered:?}"),
        }
        app.update(key(KeyCode::Char(action_key(preview.action))));
        app.update(Msg::Preview(Box::new(preview.clone())));
        assert!(
            app.update(key(KeyCode::Char('n'))).is_empty(),
            "any other key sends nothing"
        );
    }
    for result in &actions.results {
        let fact = runnerd_fixtures::action_fact(result);
        app.update(Msg::Acted(result.clone()));
        let (rows, _) = frame_of(&app, 300, 60);
        assert!(
            shows(&rows, &fact.text),
            "{}: {}",
            fact.text,
            rows.join("\n")
        );
    }

    let off = runnerd_fixtures::job_actions_off();
    let mut app = self::app(&off);
    open(&mut app, off.job(TIMED_OUT).unwrap());
    assert!(
        app.update(key(KeyCode::Char('x'))).is_empty(),
        "nothing is asked while job actions are off"
    );
    let (rows, _) = frame_of(&app, 300, 60);
    let why = off.status.job_actions.why_not().unwrap();
    assert!(shows(&rows, why), "{why}: {}", rows.join("\n"));
}

/// The key that asks for `action`'s preview.
fn action_key(action: JobAction) -> char {
    match action {
        JobAction::Cancel => 'x',
        JobAction::Rerun => 'a',
        JobAction::RerunFailed | JobAction::Unsupported => 'A',
    }
}

/// The preview of `action` among the fixtures'.
fn preview_of(actions: &runnerd_fixtures::JobActionFixtures, action: JobAction) -> Msg {
    let preview = actions
        .previews
        .iter()
        .find(|preview| preview.action == action)
        .unwrap();
    Msg::Preview(Box::new(preview.clone()))
}

#[test]
fn a_preview_taller_than_the_screen_keeps_its_notes_and_marks_what_it_leaves_out() {
    let daemon = daemon();
    let actions = runnerd_fixtures::job_actions(&daemon);
    let mut preview = actions
        .previews
        .iter()
        .find(|preview| !preview.siblings.is_empty())
        .unwrap()
        .clone();
    let one = preview.siblings[0].clone();
    preview.siblings = (0..30)
        .map(|n| runnerd_core::api::JobSummary {
            id: one.id + n,
            ..one.clone()
        })
        .collect();
    let mut app = app(&daemon);
    open(&mut app, daemon.job(TIMED_OUT).unwrap());
    app.update(key(KeyCode::Char(action_key(preview.action))));
    app.update(Msg::Preview(Box::new(preview.clone())));
    let (rows, _) = frame_of(&app, 80, 24);
    let all = rows.join("\n");
    assert!(
        shows(&rows, "more not shown"),
        "the jobs left out are marked:\n{all}"
    );
    let first_words = |text: &str| {
        text.split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
    };
    for note in &preview.notes {
        assert!(shows(&rows, &first_words(note)), "{note}:\n{all}");
    }
    assert!(
        shows(&rows, &first_words(&preview.siblings_note)),
        "{}:\n{all}",
        preview.siblings_note
    );
}

#[test]
fn a_preview_is_shown_only_on_its_job_and_only_for_the_action_asked_last() {
    let daemon = daemon();
    let actions = runnerd_fixtures::job_actions(&daemon);
    let job = daemon.job(TIMED_OUT).unwrap();
    let mut app = app(&daemon);
    open(&mut app, job);
    app.update(key(KeyCode::Char('x')));
    let (rows, _) = frame_of(&app, 120, 32);
    assert!(
        shows(&rows, "reading what it would do"),
        "the footer says the preview is read: {}",
        rows.join("\n")
    );

    // `x`, then `a` before either answers: the cancel's preview is not the one asked last.
    app.update(key(KeyCode::Char('a')));
    app.update(preview_of(&actions, JobAction::Cancel));
    assert!(
        app.prompts.top().is_none(),
        "the cancel asked first is not shown"
    );
    app.update(preview_of(&actions, JobAction::Rerun));
    assert!(
        matches!(app.prompts.top(), Some(Prompt::Action(preview)) if preview.action == JobAction::Rerun),
        "the re-run asked last is shown"
    );
    app.update(key(KeyCode::Char('n')));

    // Asked, then left: the preview that comes after is dropped and `y` sends nothing.
    app.update(key(KeyCode::Char('x')));
    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Char('o')));
    app.update(preview_of(&actions, JobAction::Cancel));
    assert!(app.prompts.top().is_none(), "no preview over the overview");
    assert!(
        app.update(key(KeyCode::Char('y'))).is_empty(),
        "y on the overview sends nothing"
    );

    // Asked on one job, answered while another is open.
    open(&mut app, job);
    app.update(key(KeyCode::Char('x')));
    app.update(key(KeyCode::Esc));
    let other = daemon
        .finished()
        .into_iter()
        .find(|summary| summary.id != job.summary.id)
        .unwrap();
    let other = daemon.job(other.id).unwrap();
    open(&mut app, other);
    app.update(preview_of(&actions, JobAction::Cancel));
    assert!(
        app.prompts.top().is_none(),
        "job {}'s preview is not shown over job {}",
        job.summary.id,
        other.summary.id
    );
}

#[test]
fn a_click_or_the_wheel_answers_a_question_and_sends_nothing() {
    use super::super::mouse::{List, Target};
    let daemon = daemon();
    let actions = runnerd_fixtures::job_actions(&daemon);
    let mut app = app(&daemon);
    open(&mut app, daemon.job(TIMED_OUT).unwrap());
    for input in [
        Msg::Click(Target::Tab(0)),
        Msg::Click(Target::Row(List::Log, 0)),
        Msg::Wheel(1),
    ] {
        app.update(key(KeyCode::Char('x')));
        app.update(preview_of(&actions, JobAction::Cancel));
        assert!(
            app.prompts.top().is_some(),
            "the cancel's question is asked"
        );
        let effects = app.update(input);
        assert!(effects.is_empty(), "the input sends nothing: {effects:?}");
        assert!(
            app.prompts.top().is_none(),
            "the input answers the question"
        );
        assert_eq!(app.view, AppView::Job, "the job stays open");
        assert!(
            app.update(key(KeyCode::Char('y'))).is_empty(),
            "a y after it sends nothing"
        );
    }
}

#[test]
fn keys_the_wheel_and_clicks_move_the_selection_in_each_followed_pane() {
    use super::super::app::Pane;
    use super::super::mouse::{List, Target};
    let daemon = daemon();
    let attention = runnerd_fixtures::attention(&daemon, &runnerd_fixtures::doctor());
    let mut app = app(&daemon);
    app.update(Msg::Attention(Ok(attention)));
    assert!(app.notices().len() >= 3, "three items to move over");
    assert!(app.live.status.queued.len() >= 3, "three queued jobs");
    assert!(app.live.recent.len() >= 3, "three latest jobs");
    let selected = |app: &App, pane: Pane| match pane {
        Pane::Attention => app.notice,
        Pane::Queue => app.queued,
        Pane::Latest => app.latest,
        Pane::Slots | Pane::Repositories => unreachable!(),
    };
    for (pane, list) in [
        (Pane::Attention, List::Attention),
        (Pane::Queue, List::Queue),
        (Pane::Latest, List::Latest),
    ] {
        while app.pane != pane {
            app.update(key(KeyCode::Tab));
        }
        let start = selected(&app, pane);
        app.update(key(KeyCode::Char('j')));
        assert_eq!(selected(&app, pane), start + 1, "j moves down {pane:?}");
        app.update(Msg::Wheel(1));
        assert_eq!(
            selected(&app, pane),
            start + 2,
            "the wheel moves down {pane:?}"
        );
        app.update(key(KeyCode::Char('k')));
        assert_eq!(selected(&app, pane), start + 1, "k moves up {pane:?}");
        app.update(Msg::Click(Target::Row(list, start)));
        assert_eq!(selected(&app, pane), start, "a click selects in {pane:?}");
    }
}

#[test]
fn an_attention_item_leads_to_its_check_or_insight_once_that_view_is_read() {
    use runnerd_core::api::Subject;
    let daemon = daemon();
    let doctor = runnerd_fixtures::doctor();
    let attention = runnerd_fixtures::attention(&daemon, &doctor);
    let insights = runnerd_fixtures::insights(NOW);
    let mut followed = 0;
    for (at, item) in attention.items.iter().enumerate() {
        let wanted = match &item.subject {
            Subject::Check { name } => doctor.checks.iter().position(|check| &check.name == name),
            Subject::Insight { key } => insights
                .insights
                .iter()
                .position(|insight| &insight.key == key),
            _ => continue,
        }
        .unwrap();
        let mut app = app(&daemon);
        app.update(Msg::Attention(Ok(attention.clone())));
        app.update(key(KeyCode::Char('!')));
        for _ in 0..at {
            app.update(key(KeyCode::Char('j')));
        }
        app.update(key(KeyCode::Enter));
        // The view's read answers after it shows.
        app.update(Msg::Doctor(Ok(doctor.clone())));
        app.update(Msg::Insights(Ok(Box::new(insights.clone()))));
        let selected = match item.subject {
            Subject::Check { .. } => (AppView::Settings, app.check),
            _ => (AppView::Insights, app.insight),
        };
        assert_eq!(selected, (app.view, wanted), "{} is selected", item.title);
        followed += 1;
    }
    assert!(followed >= 2, "the fixtures point at checks and insights");
}

#[test]
fn insights_that_fail_to_read_or_are_not_served_say_so() {
    for (unread, says) in [
        (
            super::super::reading::Unread::Failed("the history is locked".to_owned()),
            "the history is locked",
        ),
        (
            super::super::reading::Unread::Unsupported,
            runnerd_core::api::NOT_SUPPORTED,
        ),
    ] {
        let mut app = app(&daemon());
        app.update(super::tab(View::Insights));
        app.update(Msg::Insights(Err(unread)));
        let (rows, _) = frame_of(&app, 120, 32);
        assert!(shows(&rows, says), "{says}:\n{}", rows.join("\n"));
    }
}

#[test]
fn after_a_reconnect_the_open_job_its_comparison_and_the_jobs_list_are_read_again() {
    let daemon = daemon();
    let mut app = app(&daemon);
    let job = daemon.job(TIMED_OUT).unwrap();
    open(&mut app, job);
    let id = job.summary.id;
    assert_eq!(
        app.update(key(KeyCode::Char('c'))),
        vec![Effect::Compare(id)]
    );
    app.update(Msg::Compared {
        job: id,
        comparison: Err(super::super::reading::Unread::Failed(
            "timed out".to_owned(),
        )),
    });
    app.update(Msg::Disconnected("runnerd is not running".to_owned()));
    let effects = app.update(Msg::Reconnected);
    assert!(
        effects.contains(&Effect::OpenJob(id)) && effects.contains(&Effect::Compare(id)),
        "the job is opened and compared again: {effects:?}"
    );

    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Esc));
    let effects = app.update(Msg::Snapshot(Box::new(watch::State {
        status: daemon.status.clone(),
        running: BTreeMap::new(),
        samples: BTreeMap::new(),
        host: None,
        recent: daemon.finished(),
    })));
    assert_eq!(app.view, AppView::History);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::History { .. })),
        "the jobs list is read again: {effects:?}"
    );
}

#[test]
fn settings_state_each_doctor_check_and_the_state_of_its_run_as_core_does() {
    let doctors =
        std::iter::once(runnerd_fixtures::doctor()).chain(runnerd_fixtures::doctor_not_current());
    for doctor in doctors {
        let mut app = app(&daemon());
        app.update(super::tab(View::Settings));
        app.update(Msg::Doctor(Ok(doctor.clone())));
        let (rows, buffer) = frame(&app);
        let all = rows.join("\n");
        if let Some(state) = doctor.state.as_ref().and_then(|state| state.line(NOW)) {
            assert!(shows(&rows, &state), "{state}: {all}");
        }
        for check in &doctor.checks {
            let row = rows
                .iter()
                .find(|row| row.contains(&check.name) && row.contains(&check.detail))
                .unwrap_or_else(|| panic!("{}: {all}", check.name));
            assert!(row.contains(check.status.label()), "{row}");
        }
        if let Some(first) = doctor.checks.first() {
            let y = rows
                .iter()
                .position(|row| row.contains(&first.name) && row.contains(&first.detail))
                .unwrap();
            let x = rows[y][..rows[y].find(first.status.label()).unwrap()]
                .chars()
                .count();
            assert_eq!(
                Some(buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())].fg),
                COLOUR
                    .of(first.status.tone())
                    .fg
                    .or(Some(ratatui::style::Color::Reset)),
                "{} in its tone",
                first.name
            );
        }
    }
}

#[test]
fn repository_and_job_names_are_never_dim() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    let dim = |cell: &ratatui::buffer::Cell| {
        cell.fg == ratatui::style::Color::DarkGray
            || cell.modifier.contains(ratatui::style::Modifier::DIM)
    };
    let check = |app: &App, where_: &str| {
        let (rows, buffer) = frame(app);
        // Each job's row: its name and its repository's, where both stand.
        for job in daemon.finished() {
            let title = job.title().to_string();
            for (y, row) in rows.iter().enumerate() {
                if !row.contains(&title) {
                    continue;
                }
                for name in [&title, &job.repository] {
                    let Some(at) = row.find(name.as_str()) else {
                        continue;
                    };
                    let x = u16::try_from(row[..at].chars().count()).unwrap();
                    assert!(
                        !dim(&buffer[(x, u16::try_from(y).unwrap())]),
                        "{name} is not dim on the {where_}: {row}"
                    );
                }
            }
        }
    };
    check(&app, "overview");
    history(&mut app, &daemon);
    check(&app, "jobs view");
    open(&mut app, daemon.job(TIMED_OUT).unwrap());
    check(&app, "job page");
}

#[test]
fn a_click_selects_a_row_and_a_second_opens_it_and_the_wheel_moves() {
    use super::super::mouse::{List, Target};
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    history(&mut app, &daemon);
    let second = daemon.finished()[1].id;
    assert!(
        app.update(Msg::Click(Target::Row(List::History, 1)))
            .is_empty(),
        "the first click selects"
    );
    assert_eq!(app.history.selected, 1);
    assert!(
        app.update(Msg::Click(Target::Row(List::History, 1)))
            .contains(&Effect::OpenJob(second)),
        "the second opens"
    );
    app.update(key(KeyCode::Esc));
    app.update(Msg::Wheel(-1));
    assert_eq!(app.history.selected, 0, "the wheel moves the selection");
}

#[test]
fn the_jobs_table_keeps_room_for_names_at_60_columns() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    history(&mut app, &daemon);
    let (rows, _) = frame_of(&app, 60, 20);
    let job = &daemon.finished()[0];
    for name in [&job.repository, &job.title().to_string()] {
        let start: String = name.chars().take(12).collect();
        assert!(
            shows(&rows, &start),
            "{start} of {name} at 60 columns:\n{}",
            rows.join("\n")
        );
    }
}

#[test]
fn an_older_daemon_that_ignores_search_says_so_beside_the_list() {
    let (daemon, _) = fixtures();
    let mut app = app(&daemon);
    let generation = app
        .update(super::tab(View::Jobs))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::History { generation, .. } => Some(generation),
            _ => None,
        })
        .unwrap();
    let mut list = listed(JobPage {
        jobs: daemon.finished(),
        next: None,
    });
    list.applied = None;
    app.update(Msg::History {
        generation,
        list: Ok(list),
    });
    let (rows, _) = frame(&app);
    assert!(
        shows(
            &rows,
            &format!("search and sort {}", runnerd_core::api::NOT_SUPPORTED)
        ),
        "{}",
        rows.join("\n")
    );
}

/// A logged fixture job, opened with its outline read.
fn logged(app: &mut App, id: u64) -> Vec<Effect> {
    let logged = runnerd_fixtures::logged_jobs()
        .into_iter()
        .find(|logged| logged.job.summary.id == id)
        .unwrap();
    open(app, &logged.job);
    app.update(Msg::Outline {
        job: id,
        outline: Ok(logged.outline),
    })
}

#[test]
fn only_a_log_that_grows_says_it_is_followed() {
    for (id, grows) in [
        (runnerd_fixtures::LOG_RUNNING, true),
        (runnerd_fixtures::LOG_FAILED, false),
        (runnerd_fixtures::LOG_EXPIRED, false),
        (runnerd_fixtures::LOG_MISSING, false),
    ] {
        let mut app = app(&daemon());
        logged(&mut app, id);
        app.update(key(KeyCode::Char('G')));
        let (rows, _) = frame_of(&app, 160, 45);
        assert_eq!(
            shows(&rows, "following"),
            grows,
            "job {id} says following only while it grows:\n{}",
            rows.join("\n")
        );
    }
}

#[test]
fn a_cut_line_says_how_much_is_not_shown_at_every_width() {
    let id = runnerd_fixtures::LOG_FAILED;
    let fact = runnerd_fixtures::log_facts()
        .into_iter()
        .find(|fact| fact.job == id)
        .unwrap();
    let cut = fact
        .lines
        .iter()
        .find(|line| line.omitted > 0)
        .expect("the fixture cuts a line");
    let words = cut.omitted_words().unwrap();
    for width in [60, 80, 160] {
        let mut app = app(&daemon());
        logged(&mut app, id);
        app.update(Msg::Lines {
            job: id,
            lines: fact.lines.clone(),
        });
        let (rows, _) = frame_of(&app, width, 90);
        assert!(
            shows(&rows, &words),
            "{words} at {width}:\n{}",
            rows.join("\n")
        );
    }
}

#[test]
fn a_page_of_the_log_that_failed_to_read_is_asked_for_again() {
    let mut app = app(&daemon());
    let id = runnerd_fixtures::LOG_FAILED;
    let asked = logged(&mut app, id);
    let range = asked
        .iter()
        .find_map(|effect| match effect {
            Effect::Lines { range, .. } => Some(*range),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the outline asks for lines: {asked:?}"));
    app.update(Msg::LinesFailed {
        job: id,
        range,
        why: "connection reset".to_owned(),
    });
    let again = app.update(Msg::Lines {
        job: id,
        lines: Vec::new(),
    });
    assert!(
        again.contains(&Effect::Lines { job: id, range }),
        "{range:?} is asked again: {again:?}"
    );
}

#[test]
fn a_failed_job_s_log_opens_at_its_failed_step_and_enter_folds_it() {
    let logged = runnerd_fixtures::logged_jobs()
        .into_iter()
        .find(|logged| logged.job.summary.id == runnerd_fixtures::LOG_FAILED)
        .unwrap();
    let failure = logged.outline.failure.clone().unwrap();
    let step = logged
        .job
        .steps
        .iter()
        .find(|step| Some(step.number) == failure.step)
        .unwrap();
    // Its number and name, as its header shows them: two steps may share a name.
    let failed = format!("{:>2} {}", step.number, step.name);
    let fact = runnerd_fixtures::log_facts()
        .into_iter()
        .find(|fact| fact.job == runnerd_fixtures::LOG_FAILED)
        .unwrap();
    let mut app = app(&daemon());
    open(&mut app, &logged.job);
    let id = logged.job.summary.id;
    app.update(Msg::Outline {
        job: id,
        outline: Ok(logged.outline.clone()),
    });
    app.update(Msg::Lines {
        job: id,
        lines: fact.lines.clone(),
    });
    let header = |app: &App| {
        let (rows, _) = frame_of(app, 200, 70);
        rows.into_iter()
            .find(|row| (row.starts_with("▾ ") || row.starts_with("▸ ")) && row.contains(&failed))
            .unwrap_or_else(|| panic!("{failed} has a header"))
    };
    assert!(
        header(&app).starts_with("▾ "),
        "the failed step opens: {}",
        header(&app)
    );
    let (rows, _) = frame_of(&app, 200, 70);
    let open = rows.iter().filter(|row| row.starts_with("▾ ")).count();
    assert_eq!(open, 1, "only the failed step opens:\n{}", rows.join("\n"));
    app.update(key(KeyCode::Enter));
    assert!(
        header(&app).starts_with("▸ "),
        "Enter on its header folds it: {}",
        header(&app)
    );
}
