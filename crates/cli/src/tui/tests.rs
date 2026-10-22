//! The TUI through its reducer and `TestBackend` frames: a job moving through its slot, steps and the latest jobs;
//! every pane at 80×24; no colour with `NO_COLOR`; a refused pause forced only on `y`; the history's filters; and,
//! against a fake daemon, an event stream that drops, shows "reconnecting" and resumes after the last event ID.

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
    LogSummary, ProfileStatus, RepositoryStatus, RunnerStatus, Sample, SlotStatus, Status, Step,
    Timeline,
};

use super::app::{App, Connection, Effect, Msg, View};
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
            },
            profiles: vec![profile("jvm", 2, 9 << 30), profile("node", 1, 7 << 30)],
            repositories: vec![
                repository("halyard/ledger", "jvm"),
                repository("halyard/storefront", "node"),
            ],
        },
        queued: BTreeMap::new(),
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
        for x in 0..width {
            text.push_str(buffer[(x, y)].symbol());
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
    assert!(overview.contains("mem 4.0G"), "{overview}");
    assert!(overview.contains("1m 31s"), "ran since 1009: {overview}");

    // Enter on the selected slot opens its job.
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::OpenJob(42)]);
    assert_eq!(app.view, View::Job);
    app.update(Msg::LogLine {
        job: 42,
        text: "##[group]Unit tests".to_owned(),
    });
    let job = screen(&app, 120, 40, false);
    assert!(
        job.contains("CI / build") || job.contains("build  RUNNING"),
        "{job}"
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
        "capped at 9.0G and 8.00 cores",
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
            repository: "halyard/ledger".to_owned(),
            force: false
        }]
    );
    app.update(Msg::Busy {
        repository: "halyard/ledger".to_owned(),
        message: "halyard/ledger runs job 42 on runnerd-ab12-jvm-1".to_owned(),
    });
    assert!(screen(&app, 120, 40, false).contains("runs job 42"));
    assert_eq!(app.update(key(KeyCode::Char('n'))), Vec::new());
    assert!(app.confirm.is_none());

    app.update(Msg::Busy {
        repository: "halyard/ledger".to_owned(),
        message: "halyard/ledger runs job 42".to_owned(),
    });
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::Pause {
            repository: "halyard/ledger".to_owned(),
            force: true
        }]
    );
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
