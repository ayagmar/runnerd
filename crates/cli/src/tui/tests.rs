//! The TUI through its reducer and `TestBackend` frames: a job moving through its slot, steps and the latest jobs;
//! every pane at 80×24; text at 80 to 136 columns that drops whole fields or ends in `…` rather than being cut
//! mid-word; no colour with `NO_COLOR`; a refused pause forced only on `y`; repositories changed elsewhere; the
//! history's filters; and, against a fake daemon, an event stream that drops, shows "reconnecting" and resumes
//! after the last event ID.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking"
)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State as Shared;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{StreamExt, stream};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::Color;
use runnerd_core::api::{
    Budget, Event, EventEnvelope, EventQuery, Job, JobMetrics, JobRunner, JobState, JobSummary,
    LogSummary, ProfileStatus, QueuedJob, RepositoryStatus, RunnerStatus, Sample, SeedMemory,
    SlotStatus, Status, Step, Timeline,
};

use unicode_width::UnicodeWidthStr;

use super::app::{App, Connection, Effect, Msg, Refused, View};
use super::draw::{Theme, draw};
use super::feed;
use crate::testing::Fake;
use crate::watch;

const NOW: u64 = 1100;

fn profile(name: &str, slots: usize, memory: u64) -> ProfileStatus {
    ProfileStatus {
        name: name.to_owned(),
        image: format!("runnerd-runner:{name}"),
        cpus: 8,
        memory,
        reseeding: false,
        slots: (1..=slots)
            .map(|slot| SlotStatus { slot, runner: None })
            .collect(),
    }
}

fn repository(name: &str, profile: &str) -> RepositoryStatus {
    RepositoryStatus {
        name: name.to_owned(),
        profile: profile.to_owned(),
        labels: vec!["self-hosted".to_owned()],
        paused: false,
        queued_jobs: 0,
    }
}

/// A host with a jvm profile of two 9G slots and a node profile of one 7G slot, nothing running.
fn app() -> App {
    let live = watch::State {
        status: Status {
            version: "0.0.1".to_owned(),
            shutting_down: false,
            budget: Budget {
                slice: "ci.slice".to_owned(),
                memory_max: Some(33 << 30),
                memory_in_use: 0,
                mirror_memory: 0,
                seed_memory: Vec::new(),
                shell_memory: Vec::new(),
            },
            profiles: vec![profile("jvm", 2, 9 << 30), profile("node", 1, 7 << 30)],
            repositories: vec![
                repository("halyard/ledger", "jvm"),
                repository("halyard/storefront", "node"),
            ],
            queued: Vec::new(),
        },
        running: BTreeMap::new(),
        samples: BTreeMap::new(),
        host: None,
        recent: Vec::new(),
    };
    App::new(live, Vec::new(), Vec::new())
}

fn summary(id: u64, state: JobState) -> JobSummary {
    JobSummary {
        id,
        repository: "halyard/ledger".to_owned(),
        run_id: 4001,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: "build".to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        labels: vec!["self-hosted".to_owned()],
        state,
        conclusion: None,
        runner: (state != JobState::Queued).then(|| JobRunner {
            name: "runnerd-ab12-jvm-1".to_owned(),
            profile: "jvm".to_owned(),
            slot: 1,
            memory_cap: Some(9 << 30),
            cpu_cap: Some(8000),
        }),
        timeline: Timeline {
            queued_at: 1000,
            container_started_at: (state != JobState::Queued).then_some(1004),
            started_at: (state != JobState::Queued).then_some(1009),
            ..Timeline::default()
        },
        metrics: JobMetrics::default(),
        log: LogSummary::default(),
    }
}

fn step(number: u32, name: &str, started: u64, completed: Option<u64>) -> Step {
    Step {
        number,
        name: name.to_owned(),
        status: if completed.is_some() {
            "completed"
        } else {
            "in_progress"
        }
        .to_owned(),
        conclusion: completed.map(|_| "success".to_owned()),
        started_at: Some(started),
        completed_at: completed,
    }
}

fn runner(busy: bool) -> RunnerStatus {
    RunnerStatus {
        repository: "halyard/ledger".to_owned(),
        name: "runnerd-ab12-jvm-1".to_owned(),
        runner_id: 3,
        container: "c0ffee".to_owned(),
        busy,
        started_at: 1004,
    }
}

fn event(id: Option<u64>, event: Event) -> Msg {
    Msg::Event(EventEnvelope { id, at: NOW, event })
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::from(code))
}

/// The screen as text, one line per row.
fn screen(app: &App, width: u16, height: u16, colour: bool) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| draw(frame, app, NOW, Theme { colour }))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..height {
        let mut x = 0;
        while x < width {
            // A wide character covers the cell after it, which ratatui leaves blank.
            let symbol = buffer[(x, y)].symbol();
            text.push_str(symbol);
            x += u16::try_from(symbol.width()).unwrap().max(1);
        }
        text.push('\n');
    }
    text
}

/// A job of halyard/ledger queued, taken by jvm slot 1, through two steps of a three-step job, with samples.
fn running_job(app: &mut App) {
    let steps = [
        event(
            Some(1),
            Event::JobQueued {
                job: summary(42, JobState::Queued),
            },
        ),
        event(
            Some(2),
            Event::SlotChanged {
                profile: "jvm".to_owned(),
                slot: SlotStatus {
                    slot: 1,
                    runner: Some(runner(true)),
                },
            },
        ),
        event(
            Some(3),
            Event::JobStarted {
                job: summary(42, JobState::Running),
            },
        ),
        event(
            Some(4),
            Event::JobStep {
                job: 42,
                step: step(1, "Set up job", 1009, Some(1012)),
            },
        ),
        event(
            Some(5),
            Event::JobStep {
                job: 42,
                step: step(2, "Unit tests", 1012, None),
            },
        ),
        event(
            Some(6),
            Event::JobStep {
                job: 42,
                step: Step {
                    started_at: None,
                    status: "queued".to_owned(),
                    conclusion: None,
                    ..step(3, "Complete job", 0, None)
                },
            },
        ),
    ];
    for msg in steps {
        app.update(msg);
    }
    for (at, memory) in [(1090, 2 << 30), (1092, 3 << 30), (1094, 4 << 30)] {
        app.update(event(
            None,
            Event::JobSample {
                job: 42,
                sample: Sample {
                    at,
                    cpu: 3140,
                    memory,
                    network_received: 0,
                    network_sent: 0,
                },
            },
        ));
    }
}

#[test]
fn a_job_moves_through_its_slot_its_steps_and_into_the_latest_jobs() {
    let mut app = app();
    running_job(&mut app);

    let overview = screen(&app, 120, 40, false);
    assert!(overview.contains("jvm · slot 1"), "{overview}");
    assert!(overview.contains("CI / build"), "{overview}");
    assert!(overview.contains("2/3 Unit tests"), "{overview}");
    assert!(overview.contains("mem 4G"), "{overview}");
    assert!(overview.contains("1m 31s"), "ran since 1009: {overview}");

    // Enter on the selected slot opens its job.
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::OpenJob(42)]);
    assert_eq!(app.view, View::Job);
    app.update(Msg::LogLine {
        job: 42,
        text: "##[group]Unit tests".to_owned(),
    });
    let job = screen(&app, 120, 40, false);
    let head: Vec<&str> = job.lines().skip(1).take(2).map(str::trim_end).collect();
    assert_eq!(
        head,
        ["■ halyard/ledger", "CI / build  RUNNING"],
        "the repository, then the job as every other view names it: {job}"
    );
    for expected in [
        "Set up job",
        "3s",
        "Unit tests",
        "1m 28s…",
        "Complete job",
        "–",
        "waiting for a runner 4s",
        "runner starting 5s",
        "capped at 9G and 8 CPUs",
        "##[group]Unit tests",
    ] {
        assert!(job.contains(expected), "{expected}: {job}");
    }

    let mut finished = summary(42, JobState::Completed);
    finished.conclusion = Some("success".to_owned());
    finished.timeline.finished_at = Some(1095);
    // The finished job is read again, for its steps' last state and its totals.
    assert_eq!(
        app.update(event(Some(7), Event::JobFinished { job: finished })),
        vec![Effect::RefreshJob(42)]
    );
    assert!(screen(&app, 120, 40, false).contains("SUCCESS"));
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::CloseJob]);
    let overview = screen(&app, 120, 40, false);
    assert!(overview.contains("SUCCESS"), "{overview}");
    assert!(app.live.running.is_empty() && app.series.is_empty());
}

#[test]
fn every_pane_fits_a_terminal_of_80_by_24() {
    let mut app = app();
    running_job(&mut app);
    let overview = screen(&app, 80, 24, true);
    for expected in [
        "Budget",
        "33G reserved in ci.slice",
        "Slots",
        "jvm 1",
        "jvm 2",
        "node 1",
        "2/3 Unit tests",
        "Memory",
        "CPU",
        "Repositories",
        "halyard/storefront",
        "Latest jobs",
        "q quit",
    ] {
        assert!(overview.contains(expected), "{expected}: {overview}");
    }

    app.update(key(KeyCode::Enter));
    for line in 0..100 {
        app.update(Msg::LogLine {
            job: 42,
            text: format!("line {line}"),
        });
    }
    let job = screen(&app, 80, 24, true);
    for expected in [
        "Set up job",
        "Unit tests",
        "Complete job",
        "mem",
        "Log",
        "line 99",
    ] {
        assert!(job.contains(expected), "{expected}: {job}");
    }

    let (generation, _) = history_request(&app.update(key(KeyCode::Char('2'))));
    app.update(history_page(
        generation,
        vec![summary(42, JobState::Running)],
        None,
    ));
    let history = screen(&app, 80, 24, true);
    assert!(
        history.contains("OUTCOME") && history.contains("RUNNING"),
        "{history}"
    );

    // Smaller still: nothing panics, the header and footer stay.
    app.update(key(KeyCode::Char('1')));
    let tiny = screen(&app, 30, 6, true);
    assert!(tiny.contains("runnerd"), "{tiny}");
}

#[test]
fn without_colour_nothing_is_coloured() {
    let mut app = app();
    running_job(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| draw(frame, &app, NOW, Theme { colour: false }))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
    );
}

#[test]
fn a_refused_pause_is_forced_only_after_y() {
    let mut app = app();
    running_job(&mut app);
    // The selected slot runs a job of halyard/ledger.
    assert_eq!(
        app.update(key(KeyCode::Char('p'))),
        vec![Effect::Pause {
            repository: Some("halyard/ledger".to_owned()),
            force: false
        }]
    );
    app.update(Msg::Busy(Refused {
        repository: Some("halyard/ledger".to_owned()),
        message: "halyard/ledger runs job 42 on runnerd-ab12-jvm-1".to_owned(),
        jobs: 1,
    }));
    assert!(screen(&app, 120, 40, false).contains("runs job 42"));
    assert_eq!(app.update(key(KeyCode::Char('n'))), Vec::new());
    assert!(app.confirm.is_none());

    app.update(Msg::Busy(Refused {
        repository: Some("halyard/ledger".to_owned()),
        message: "halyard/ledger runs job 42".to_owned(),
        jobs: 1,
    }));
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::Pause {
            repository: Some("halyard/ledger".to_owned()),
            force: true
        }]
    );
}

#[test]
fn queued_jobs_lead_the_latest_jobs_oldest_first_and_open_and_pause_like_the_others() {
    let mut app = app();
    let queued = |id, name: &str, at| QueuedJob {
        id,
        repository: "halyard/storefront".to_owned(),
        run_id: 4002,
        run_attempt: 1,
        workflow: Some("E2E".to_owned()),
        name: name.to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        queued_at: at,
    };
    app.live.status.queued = vec![queued(51, "visual-diff", 1041), queued(52, "a11y", 1090)];
    app.live.recent = vec![summary(42, JobState::Completed)];
    let shown = screen(&app, 120, 40, false);
    let rows: Vec<&str> = shown
        .lines()
        .filter_map(|line| {
            let at = line.find("QUEUED").or_else(|| line.find("COMPLETED"))?;
            Some(line[at..].trim_end())
        })
        .collect();
    assert_eq!(rows.len(), 3, "{shown}");
    assert!(
        rows[0].starts_with("QUEUED    E2E / visual-diff halyard/storefront")
            && rows[0].ends_with("59s ago"),
        "{rows:?}"
    );
    assert!(rows[1].starts_with("QUEUED    E2E / a11y"), "{rows:?}");
    assert!(rows[2].contains("CI / build halyard/ledger"), "{rows:?}");

    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    assert_eq!(
        app.update(key(KeyCode::Char('p'))),
        vec![Effect::Pause {
            repository: Some("halyard/storefront".to_owned()),
            force: false
        }]
    );
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::OpenJob(51)]);
    let job = screen(&app, 120, 40, false);
    let head: Vec<&str> = job.lines().skip(1).take(2).map(str::trim_end).collect();
    assert_eq!(
        head,
        ["■ halyard/storefront", "E2E / visual-diff  QUEUED"],
        "{job}"
    );
}

#[test]
fn the_selected_latest_job_stays_selected_when_a_queued_job_above_it_starts() {
    let mut app = app();
    let queued = |id, repository: &str| QueuedJob {
        id,
        repository: repository.to_owned(),
        run_id: 4002,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: "build".to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        queued_at: 1000,
    };
    app.live.status.queued = vec![
        queued(51, "halyard/ledger"),
        queued(52, "halyard/storefront"),
    ];
    app.live.recent = vec![summary(42, JobState::Completed)];
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Down));
    app.update(event(
        Some(9),
        Event::JobStarted {
            job: summary(51, JobState::Running),
        },
    ));
    assert_eq!(
        app.update(key(KeyCode::Char('p'))),
        vec![Effect::Pause {
            repository: Some("halyard/storefront".to_owned()),
            force: false
        }]
    );
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::OpenJob(52)]);

    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Down));
    app.update(event(
        Some(10),
        Event::JobStarted {
            job: summary(52, JobState::Running),
        },
    ));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::OpenJob(42)],
        "a selection whose job left the list moves to a row still there"
    );
}

#[test]
fn the_budget_names_the_mirror_and_each_seed_container() {
    let mut app = app();
    let budget = &mut app.live.status.budget;
    budget.mirror_memory = 4 << 30;
    budget.seed_memory = vec![SeedMemory {
        profile: "jvm".to_owned(),
        memory: 9 << 30,
    }];
    budget.memory_in_use = 13 << 30;
    let shown = screen(&app, 160, 40, false);
    let names = shown.lines().nth(3).unwrap();
    assert!(
        names.contains("mirror 4G") && names.contains("jvm seed 9G"),
        "{shown}"
    );

    // A 512M mirror is a cell or two at common widths: the heading names it.
    app.live.status.budget.mirror_memory = 512 << 20;
    let shown = screen(&app, 120, 40, false);
    let mut lines = shown.lines().skip(1);
    let (heading, bar) = (lines.next().unwrap(), lines.next().unwrap());
    assert!(heading.ends_with("· mirror 512M"), "{shown}");
    assert!(bar.contains("▒ "), "the mirror's block shows: {shown}");

    // Without a MemoryMax the slots fill the bar, leaving no room for a block: the heading still names each.
    app.live.status.budget.memory_max = None;
    let shown = screen(&app, 120, 40, false);
    let heading = shown.lines().nth(1).unwrap();
    assert!(heading.contains("· mirror 512M · jvm seed 9G"), "{shown}");
}

#[test]
fn a_profile_a_reload_adds_or_removes_comes_and_goes_and_the_selection_stays_on_a_slot() {
    let mut app = app();
    app.update(event(
        Some(5),
        Event::ProfileChanged {
            profile: profile("rust", 1, 4 << 30),
        },
    ));
    assert!(
        screen(&app, 120, 40, false).contains("rust · slot 1"),
        "the new profile's slot shows"
    );
    for _ in 0..3 {
        app.update(key(KeyCode::Char('j')));
    }
    app.update(event(
        Some(6),
        Event::ProfileRemoved {
            profile: "rust".to_owned(),
        },
    ));
    assert!(
        !screen(&app, 120, 40, false).contains("rust · slot 1"),
        "the removed profile's slot is gone"
    );
    assert_eq!(
        app.update(key(KeyCode::Char('R'))),
        vec![Effect::Reseed {
            profile: "node".to_owned()
        }],
        "the selection moved to the last slot left"
    );
}

#[test]
fn shift_p_and_u_pause_and_resume_every_repository_and_a_refused_pause_of_all_is_forced_after_y() {
    let mut app = app();
    assert_eq!(
        app.update(key(KeyCode::Char('P'))),
        vec![Effect::Pause {
            repository: None,
            force: false
        }]
    );
    app.update(Msg::Busy(Refused {
        repository: None,
        message: "halyard/ledger runs job 120055991700 on runnerd-3f9a-forge-jvm-1-1792721996; \
                  halyard/storefront runs job 120055991650 on runnerd-3f9a-forge-node-1-1792722114"
            .to_owned(),
        jobs: 2,
    }));
    let asked = screen(&app, 136, 40, false);
    let prompt = asked.lines().last().unwrap();
    assert!(
        prompt.starts_with(
            " y cancels the runs of 2 jobs and pauses, any other key keeps them running: "
        ),
        "{prompt}"
    );
    assert!(
        prompt.trim_end().ends_with('…'),
        "a message too long for the line is cut with …: {prompt}"
    );
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::Pause {
            repository: None,
            force: true
        }]
    );
    assert_eq!(
        app.update(key(KeyCode::Char('U'))),
        vec![Effect::Resume { repository: None }]
    );
}

#[test]
fn repositories_paused_added_or_removed_elsewhere_show_without_reading_the_status() {
    let mut app = app();
    let mut storefront = repository("halyard/storefront", "node");
    storefront.paused = true;
    storefront.queued_jobs = 3;
    app.update(event(
        Some(1),
        Event::RepositoryChanged {
            repository: storefront,
        },
    ));
    app.update(event(
        Some(2),
        Event::RepositoryChanged {
            repository: repository("halyard/docs", "node"),
        },
    ));
    app.update(event(
        Some(3),
        Event::RepositoryRemoved {
            repository: "halyard/ledger".to_owned(),
        },
    ));
    let shown = screen(&app, 120, 40, false);
    assert!(
        shown.contains("halyard/storefront  node  3 queued  paused"),
        "{shown}"
    );
    assert!(shown.contains("halyard/docs  node  0 queued"), "{shown}");
    assert!(!shown.contains("halyard/ledger"), "{shown}");
}

/// Records each event stream request's `Last-Event-ID`. The first stream sends event 7 and ends; the second
/// request fails, so the stream fails for good; the third serves event 8.
async fn dropping_then_back(
    Shared(seen): Shared<Arc<Mutex<Vec<Option<String>>>>>,
    headers: HeaderMap,
) -> Response {
    let request = {
        let mut seen = seen.lock().unwrap();
        seen.push(
            headers
                .get("last-event-id")
                .map(|id| id.to_str().unwrap().to_owned()),
        );
        seen.len()
    };
    let budget = |id: u64, in_use: u64| {
        let envelope = EventEnvelope {
            id: Some(id),
            at: NOW,
            event: Event::BudgetChanged {
                budget: Budget {
                    slice: "ci.slice".to_owned(),
                    memory_max: None,
                    memory_in_use: in_use,
                    mirror_memory: 0,
                    seed_memory: Vec::new(),
                    shell_memory: Vec::new(),
                },
            },
        };
        format!(
            "event: budget.changed\nid: {id}\ndata: {}\n\n",
            serde_json::to_string(&envelope).unwrap()
        )
    };
    match request {
        1 => budget(7, 70).into_response(),
        2 => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        _ => Body::from_stream(
            stream::iter([Ok::<_, Infallible>(Bytes::from(budget(8, 80)))])
                .chain(stream::pending()),
        )
        .into_response(),
    }
}

#[tokio::test]
async fn a_dropped_event_stream_shows_reconnecting_and_resumes_after_the_last_event() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake::serve(
        Router::new()
            .route("/v1/events", get(dropping_then_back))
            .with_state(seen.clone()),
    );
    let client = fake.client(Duration::from_secs(5), Duration::from_secs(5));
    let events = client.events(EventQuery::default(), None).await.unwrap();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(feed(client, events, send, Duration::from_millis(50)));

    let mut app = app();
    let mut saw_reconnecting = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.live.status.budget.memory_in_use != 80 {
            app.update(receive.recv().await.unwrap());
            if let Connection::Reconnecting(_) = app.connection {
                saw_reconnecting = true;
                assert!(screen(&app, 100, 24, false).contains("reconnecting"));
            }
        }
    })
    .await
    .expect("the feed should come back");
    assert!(saw_reconnecting);
    assert_eq!(app.connection, Connection::Live);
    assert!(screen(&app, 100, 24, false).contains("live"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.first(), Some(&None));
    assert_eq!(seen.last(), Some(&Some("7".to_owned())));
}

/// The history request among the effects: its generation and query.
fn history_request(effects: &[Effect]) -> (u64, runnerd_core::api::JobQuery) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::History { generation, query } => Some((*generation, query.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a history request, not {effects:?}"))
}

fn history_query(effects: &[Effect]) -> runnerd_core::api::JobQuery {
    history_request(effects).1
}

/// The daemon's answer to a history request.
fn history_page(generation: u64, jobs: Vec<JobSummary>, next: Option<u64>) -> Msg {
    Msg::History {
        generation,
        page: Ok(runnerd_core::api::JobPage { jobs, next }),
    }
}

#[test]
fn the_history_filters_ask_for_the_first_page_again() {
    let mut app = app();
    let (generation, first) = history_request(&app.update(key(KeyCode::Char('2'))));
    assert_eq!((first.conclusion, first.before), (None, None));
    app.update(history_page(
        generation,
        vec![summary(42, JobState::Completed)],
        Some(42),
    ));
    let next = history_query(&app.update(key(KeyCode::Char(']'))));
    assert_eq!(next.before, Some(42));
    let failed = history_query(&app.update(key(KeyCode::Char('c'))));
    assert_eq!(failed.conclusion.as_deref(), Some("failure"));
    assert_eq!(failed.before, None, "a new filter starts at the first page");
    let ledger = history_query(&app.update(key(KeyCode::Char('s'))));
    assert_eq!(ledger.repository.as_deref(), Some("halyard/ledger"));
    assert_eq!(ledger.conclusion.as_deref(), Some("failure"));
}

#[test]
fn opening_a_job_from_the_latest_jobs_follows_only_its_log() {
    let mut app = app();
    let mut done = summary(41, JobState::Completed);
    done.conclusion = Some("failure".to_owned());
    done.timeline.finished_at = Some(1050);
    app.update(event(Some(1), Event::JobFinished { job: done }));
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::OpenJob(41)]);
    app.update(Msg::LogLine {
        job: 99,
        text: "another job's line".to_owned(),
    });
    app.update(Msg::LogLine {
        job: 41,
        text: "##[error]Process completed with exit code 1.".to_owned(),
    });
    app.update(Msg::Job(Job {
        summary: app.job.as_ref().unwrap().job.summary.clone(),
        steps: vec![step(1, "Set up job", 1009, Some(1012))],
        record: None,
    }));
    let job = screen(&app, 120, 40, false);
    assert!(job.contains("FAILURE"), "{job}");
    assert!(job.contains("exit code 1"), "{job}");
    assert!(!job.contains("another job's line"), "{job}");
}

fn sample(at: u64) -> Sample {
    Sample {
        at,
        cpu: 1000,
        memory: at,
        network_received: 0,
        network_sent: 0,
    }
}

#[test]
fn an_open_job_keeps_only_its_newest_samples() {
    let mut app = app();
    running_job(&mut app);
    app.update(key(KeyCode::Enter));
    // A job that has run for hours: far more samples than the overview keeps.
    app.update(Msg::Samples(runnerd_core::api::SampleSeries {
        job: 42,
        interval: 2,
        samples: (0..5000).map(sample).collect(),
    }));
    for at in 5000..6000 {
        app.update(event(
            None,
            Event::JobSample {
                job: 42,
                sample: sample(at),
            },
        ));
    }
    let samples = &app.job.as_ref().unwrap().samples;
    assert_eq!(samples.len(), 300);
    assert_eq!(samples.iter().last().map(|sample| sample.at), Some(5999));
    assert_eq!(samples.iter().next().map(|sample| sample.at), Some(5700));
}

#[test]
fn a_late_answer_to_an_earlier_history_filter_is_ignored() {
    let mut app = app();
    let (first, _) = history_request(&app.update(key(KeyCode::Char('2'))));
    app.update(history_page(
        first,
        vec![summary(40, JobState::Completed)],
        Some(40),
    ));
    let (failures, _) = history_request(&app.update(key(KeyCode::Char('c'))));
    // The rows and their continuation belong to the old filter: no next page while the new one loads.
    assert_eq!(app.update(key(KeyCode::Char(']'))), Vec::new());
    let (successes, query) = history_request(&app.update(key(KeyCode::Char('c'))));
    assert_eq!(query.conclusion.as_deref(), Some("success"));

    // The answers come back in the other order.
    let mut succeeded = summary(43, JobState::Completed);
    succeeded.conclusion = Some("success".to_owned());
    let mut failed = summary(44, JobState::Completed);
    failed.conclusion = Some("failure".to_owned());
    app.update(history_page(successes, vec![succeeded], None));
    app.update(history_page(failures, vec![failed], Some(44)));

    let page = app.history.page.as_ref().unwrap();
    assert_eq!(page.jobs.iter().map(|job| job.id).collect::<Vec<_>>(), [43]);
    let history = screen(&app, 120, 40, false);
    assert!(history.contains("outcome success"), "{history}");
    assert!(
        history.contains("SUCCESS") && !history.contains("FAILURE"),
        "{history}"
    );
}

/// A daemon where job 42 runs until the event stream breaks, and has finished by the time it is back. The first
/// stream sends only a live sample, so the client has no event ID to resume from.
#[derive(Default)]
struct Outage {
    streams: std::sync::atomic::AtomicU32,
    /// `Last-Event-ID` of each stream opened after the outage.
    resumed_after: Mutex<Vec<Option<String>>>,
}

impl Outage {
    fn finished(&self) -> bool {
        self.streams.load(std::sync::atomic::Ordering::Relaxed) > 1
    }
}

async fn outage_events(Shared(outage): Shared<Arc<Outage>>, headers: HeaderMap) -> Response {
    let stream = outage
        .streams
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    match stream {
        0 => {
            let envelope = EventEnvelope {
                id: None,
                at: NOW,
                event: Event::HostSample {
                    sample: runnerd_core::api::HostSample {
                        at: NOW,
                        cpu: 10,
                        memory: 1 << 20,
                    },
                },
            };
            format!(
                "event: host.sample\ndata: {}\n\n",
                serde_json::to_string(&envelope).unwrap()
            )
            .into_response()
        }
        1 => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        _ => {
            outage.resumed_after.lock().unwrap().push(
                headers
                    .get("last-event-id")
                    .map(|id| id.to_str().unwrap().to_owned()),
            );
            Body::from_stream(stream::pending::<Result<Bytes, Infallible>>()).into_response()
        }
    }
}

async fn outage_jobs(
    Shared(outage): Shared<Arc<Outage>>,
    axum::extract::Query(query): axum::extract::Query<runnerd_core::api::JobQuery>,
) -> axum::Json<runnerd_core::api::JobPage> {
    let job = if outage.finished() {
        let mut done = summary(42, JobState::Completed);
        done.conclusion = Some("success".to_owned());
        done.timeline.finished_at = Some(1095);
        done
    } else {
        summary(42, JobState::Running)
    };
    let jobs = match query.state {
        None => vec![job],
        Some(state) if state == job.state => vec![job],
        Some(_) => Vec::new(),
    };
    axum::Json(runnerd_core::api::JobPage { jobs, next: None })
}

#[tokio::test]
async fn a_reconnect_with_no_event_to_resume_after_reads_the_state_again() {
    let outage = Arc::new(Outage::default());
    let fake = Fake::serve(
        Router::new()
            .route("/v1/events", get(outage_events))
            .route(
                "/v1/status",
                get(|| async { axum::Json(app().live.status) }),
            )
            .route("/v1/jobs", get(outage_jobs))
            .with_state(outage.clone()),
    );
    let client = fake.client(Duration::from_secs(5), Duration::from_secs(5));
    let events = client.events(EventQuery::default(), None).await.unwrap();
    let mut app = app();
    running_job(&mut app);
    assert!(app.live.running.contains_key(&42));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(feed(client, events, send, Duration::from_millis(50)));

    let mut was_reconnecting = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            app.update(receive.recv().await.unwrap());
            match app.connection {
                Connection::Reconnecting(_) => was_reconnecting = true,
                Connection::Live if was_reconnecting => break,
                Connection::Live => {}
            }
        }
    })
    .await
    .expect("the feed should come back");
    // Let anything sent with the reconnect arrive.
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(msg) = receive.try_recv() {
        app.update(msg);
    }
    assert_eq!(*outage.resumed_after.lock().unwrap(), [None]);
    assert!(
        app.live.running.is_empty(),
        "job 42 finished during the outage"
    );
    assert_eq!(
        app.live
            .recent
            .first()
            .map(|job| (job.id, job.conclusion.as_deref())),
        Some((42, Some("success")))
    );
    let overview = screen(&app, 120, 40, false);
    assert!(overview.contains("SUCCESS"), "{overview}");
    assert!(!overview.contains("Unit tests"), "{overview}");
}

#[test]
fn a_job_read_again_as_finished_leaves_the_slots() {
    let mut app = app();
    running_job(&mut app);
    app.update(key(KeyCode::Enter));
    let mut done = summary(42, JobState::Completed);
    done.conclusion = Some("success".to_owned());
    app.update(Msg::Job(Job {
        summary: done,
        steps: Vec::new(),
        record: None,
    }));
    assert!(app.live.running.is_empty() && app.series.is_empty());
    assert!(screen(&app, 120, 40, false).contains("SUCCESS"));
}

/// halyard/ledger's job `id`, which succeeded `ago` seconds before now.
fn finished(id: u64, ago: u64) -> JobSummary {
    let mut job = summary(id, JobState::Completed);
    job.conclusion = Some("success".to_owned());
    job.timeline.finished_at = Some(NOW - ago);
    job.timeline.started_at = Some(0);
    job
}

/// The rows of `screen` that contain `text`.
fn rows_with<'screen>(screen: &'screen str, text: &str) -> Vec<&'screen str> {
    screen.lines().filter(|row| row.contains(text)).collect()
}

#[test]
fn how_long_ago_a_job_finished_reads_in_full_at_every_width() {
    let mut app = app();
    app.live.recent = vec![finished(7, 17 * 60 + 31), finished(8, 5)];
    for width in [80, 96, 104, 120, 136] {
        let overview = screen(&app, width, 40, false);
        assert!(
            overview.contains("17m 31s ago"),
            "the age reads in full at {width}: {overview}"
        );
        assert!(
            overview.contains("5s ago"),
            "the age reads in full at {width}: {overview}"
        );
    }
    let (generation, _) = history_request(&app.update(key(KeyCode::Char('2'))));
    app.update(history_page(
        generation,
        vec![finished(7, 17 * 60 + 31)],
        None,
    ));
    for width in [80, 96, 120, 136] {
        let history = screen(&app, width, 24, false);
        assert!(
            history.contains("17m 31s ago"),
            "the age reads in full at {width}: {history}"
        );
    }
}

#[test]
fn every_slot_card_title_keeps_its_slot_number_and_a_rule() {
    let mut app = app();
    app.live.status.profiles[1] = profile("node", 2, 7 << 30);
    for width in [96, 104, 120, 136] {
        let overview = screen(&app, width, 40, false);
        for card in [
            "jvm · slot 1",
            "jvm · slot 2",
            "node · slot 1",
            "node · slot 2",
        ] {
            assert!(
                overview.contains(&format!("{card} ─")) || overview.contains(&format!("{card} ━")),
                "{card} then its rule at {width}: {overview}"
            );
        }
        // The size reads the same on every card.
        let sizes = rows_with(&overview, " · slot ")
            .iter()
            .map(|row| row.matches("CPUs").count())
            .sum::<usize>();
        assert!(
            sizes == 0 || sizes == 4,
            "every card or none shows its CPUs at {width}: {overview}"
        );
    }
}

#[test]
fn the_open_job_header_drops_whole_details_to_fit() {
    let mut app = app();
    running_job(&mut app);
    app.live.running.get_mut(&42).unwrap().summary.run_id = 40_018_663_900;
    app.update(key(KeyCode::Enter));
    let whole = [
        "branch main",
        "run 40018663900",
        "slot jvm",
        "1",
        "capped at 9G and 8 CPUs",
        "waited 9s",
        "ran 1m 31s",
    ];
    for width in [80, 96, 104, 120] {
        let job = screen(&app, width, 40, false);
        let header = rows_with(&job, "slot jvm · 1")[0].trim_end();
        assert!(
            header.ends_with("ran 1m 31s"),
            "the run time stays at {width}: {header}"
        );
        for detail in header.split(" · ") {
            assert!(
                whole.contains(&detail),
                "{detail:?} is whole at {width}: {header}"
            );
        }
    }
}

#[test]
fn text_that_does_not_fit_ends_in_an_ellipsis_and_leaves_its_neighbour_a_space() {
    let mut app = app();
    running_job(&mut app);
    let running = app.live.running.get_mut(&42).unwrap();
    running.summary.timeline.started_at = Some(NOW - (16 * 60 + 40));
    running.steps[1].name = "Integration tests (Testcontainers)".to_owned();
    app.live.status.profiles[1] = profile("node", 2, 7 << 30);
    app.live.status.repositories[1].queued_jobs = 1;
    app.live.recent = vec![finished(7, 60)];

    let small = screen(&app, 80, 24, false);
    let storefront = rows_with(&small, "halyard/storefront")[0];
    assert!(
        storefront.contains("1 queued "),
        "a space before the latest jobs: {small}"
    );
    let slot = rows_with(&small, "jvm 1  ")[0];
    assert!(
        slot.contains("… 16m 40s "),
        "a space before the run time: {small}"
    );

    let wide = screen(&app, 136, 40, false);
    let step = rows_with(&wide, "2/3 Integration tests")[0];
    assert!(
        step.contains("(Testcont…"),
        "the step's name ends in an ellipsis: {wide}"
    );

    let mut feature = finished(7, 60);
    feature.branch = Some("feat/checkout-flow".to_owned());
    let (generation, _) = history_request(&app.update(key(KeyCode::Char('2'))));
    app.update(history_page(generation, vec![feature], None));
    let history = screen(&app, 120, 24, false);
    assert!(
        history.contains(" feat/checkout… "),
        "the branch ends in an ellipsis: {history}"
    );
}

#[test]
fn wide_characters_are_fitted_by_the_cells_they_take() {
    let wide = "界".repeat(20);
    let mut app = app();
    app.live.status.profiles[1] = profile("ビルド", 2, 7 << 30);
    running_job(&mut app);
    app.live.running.get_mut(&42).unwrap().summary.branch = Some(wide.clone());

    for width in [96, 120, 136] {
        let overview = screen(&app, width, 40, false);
        for card in ["ビルド · slot 1", "ビルド · slot 2"] {
            assert!(
                overview.contains(&format!("{card} ─")),
                "{card} then its rule at {width}: {overview}"
            );
        }
    }

    app.update(key(KeyCode::Enter));
    let whole = [
        format!("branch {wide}"),
        "run 4001".to_owned(),
        "slot jvm".to_owned(),
        "1".to_owned(),
        "capped at 9G and 8 CPUs".to_owned(),
        "waited 9s".to_owned(),
        "ran 1m 31s".to_owned(),
    ];
    for width in [80, 104, 120] {
        let job = screen(&app, width, 24, false);
        let header = rows_with(&job, "slot jvm · 1")[0].trim_end();
        assert!(
            header.ends_with("ran 1m 31s"),
            "the run time stays at {width}: {header}"
        );
        for detail in header.split(" · ") {
            assert!(
                whole.iter().any(|whole| whole == detail),
                "{detail:?} is whole at {width}: {header}"
            );
        }
    }

    let mut job = finished(7, 60);
    job.repository.clone_from(&wide);
    job.branch = Some(wide.clone());
    let (generation, _) = history_request(&app.update(key(KeyCode::Char('2'))));
    app.update(history_page(generation, vec![job], None));
    let history = screen(&app, 120, 24, false);
    let row = rows_with(&history, "SUCCESS")[0];
    assert_eq!(
        row.matches("界…").count(),
        2,
        "the repository and the branch end in an ellipsis: {history}"
    );
}

#[test]
fn an_emoji_branch_cut_in_the_history_ends_in_an_ellipsis() {
    // `❤️` is a character and a variation selector: two cells, though each character alone takes one or none.
    let mut app = app();
    let mut job = finished(7, 60);
    job.branch = Some("❤️".repeat(20));
    let (generation, _) = history_request(&app.update(key(KeyCode::Char('2'))));
    app.update(history_page(generation, vec![job], None));
    for width in [120, 136] {
        let history = screen(&app, width, 24, false);
        let row = rows_with(&history, "SUCCESS")[0];
        assert!(
            row.contains("…"),
            "the branch ends in an ellipsis at {width}: {history}"
        );
    }
}

#[test]
fn the_open_job_metrics_drop_whole_figures_to_fit() {
    let mut app = app();
    running_job(&mut app);
    app.live.running.get_mut(&42).unwrap().summary.metrics = JobMetrics {
        cpu_time: Some(1_454_000),
        cpu_peak: Some(7_960),
        cpu_average: Some(3_310),
        memory_peak: Some(7 << 30),
        memory_average: Some(5 << 30),
        network_received: Some(329 << 20),
        network_sent: Some(37 << 20),
        mirror_bytes: 110 << 20,
    };
    app.update(key(KeyCode::Enter));
    let full = screen(&app, 240, 40, false);
    let figures: Vec<&str> = rows_with(&full, "mem peak")[0].trim().split("  ").collect();
    assert!(figures.contains(&"mirror 110M"), "{full}");
    for (width, height) in [(80, 24), (96, 24), (96, 30), (104, 30)] {
        let job = screen(&app, width, height, false);
        let row = rows_with(&job, "mem peak")[0].trim();
        for figure in row.split("  ") {
            assert!(
                figures.contains(&figure),
                "{figure:?} is whole at {width}×{height}: {row}"
            );
        }
    }
}

/// A daemon whose first event stream sends only a live sample and ends; the stream the client opens again at
/// once succeeds, live only. The budget changed in between, and no event says so.
async fn cursorless_gap(
    Shared(streams): Shared<Arc<Mutex<Vec<Option<String>>>>>,
    headers: HeaderMap,
) -> Response {
    let opened = {
        let mut streams = streams.lock().unwrap();
        streams.push(
            headers
                .get("last-event-id")
                .map(|id| id.to_str().unwrap().to_owned()),
        );
        streams.len()
    };
    let sample = EventEnvelope {
        id: None,
        at: NOW,
        event: Event::HostSample {
            sample: runnerd_core::api::HostSample {
                at: NOW,
                cpu: 10,
                memory: 1 << 20,
            },
        },
    };
    let data = Bytes::from(format!(
        "event: host.sample\ndata: {}\n\n",
        serde_json::to_string(&sample).unwrap()
    ));
    if opened == 1 {
        Body::from(data).into_response()
    } else {
        Body::from_stream(stream::iter([Ok::<_, Infallible>(data)]).chain(stream::pending()))
            .into_response()
    }
}

/// The status: 1G in use once a second event stream was opened, nothing before.
async fn gap_status(
    Shared(streams): Shared<Arc<Mutex<Vec<Option<String>>>>>,
) -> axum::Json<Status> {
    let mut status = app().live.status;
    if streams.lock().unwrap().len() > 1 {
        status.budget.memory_in_use = 1 << 30;
    }
    axum::Json(status)
}

#[tokio::test]
async fn a_stream_reopened_at_once_with_no_event_to_resume_after_reads_the_state_again() {
    let streams = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake::serve(
        Router::new()
            .route("/v1/events", get(cursorless_gap))
            .route("/v1/status", get(gap_status))
            .route(
                "/v1/jobs",
                get(|| async {
                    axum::Json(runnerd_core::api::JobPage {
                        jobs: Vec::new(),
                        next: None,
                    })
                }),
            )
            .with_state(streams.clone()),
    );
    let client = fake.client(Duration::from_secs(5), Duration::from_secs(5));
    let events = client.events(EventQuery::default(), None).await.unwrap();
    let mut app = app();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(feed(client, events, send, Duration::from_millis(50)));

    tokio::time::timeout(Duration::from_secs(5), async {
        while app.live.status.budget.memory_in_use != 1 << 30 {
            app.update(receive.recv().await.unwrap());
        }
    })
    .await
    .expect("the state should be read again after the gap");
    assert_eq!(app.connection, Connection::Live);
    assert!(
        screen(&app, 120, 40, false).contains("1G of 33G reserved"),
        "the reread status shows"
    );
    assert_eq!(*streams.lock().unwrap(), [None, None]);
}
