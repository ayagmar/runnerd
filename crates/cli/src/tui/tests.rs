//! The TUI through its reducer and `TestBackend` frames: a job moving through its slot, steps and the latest jobs;
//! every pane at 80×24; text at 80 to 136 columns that drops whole fields or ends in `…` rather than being cut
//! mid-word; no colour with `NO_COLOR`; a refused pause, a removed record and written sizing advice only on `y`;
//! repositories changed elsewhere; the history's filters; and, against a fake daemon, an event stream that drops, shows
//! "reconnecting" and resumes after the last event ID.

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
    Admission, AdmissionPolicy, Basis, Budget, CpuAdmission, Event, EventEnvelope, EventQuery,
    HostBudget, HostMemory, Job, JobMetrics, JobRunner, JobState, JobSummary, LogSummary,
    ProfileStatus, QueuedJob, Record, RecordLimits, Records, RepositoryStatus, RunnerStatus,
    Sample, SeedMemory, ShellMemory, SlotStatus, Status, Step, Timeline, Waiting,
};
use runnerd_core::api::{Advice, JobSizing, Peaks, ProfileSizing, Resize, Sizing, Spread, Starts};

use unicode_width::UnicodeWidthStr;

use super::app::{App, Connection, Effect, Msg, Pane, Refused, Removal, Resizing, View};
use super::draw::{Theme, draw};
use super::feed;
use crate::testing::Fake;
use crate::watch;

const NOW: u64 = 1100;

mod parity;

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
        pools: vec![runnerd_core::api::PoolStatus {
            profile: profile.to_owned(),
            labels: vec!["self-hosted".to_owned()],
            queued_jobs: 0,
        }],
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
            stopping: None,
            budget: Budget {
                admission: AdmissionPolicy::Caps,
                held: false,
                slice: "ci.slice".to_owned(),
                memory_max: Some(33 << 30),
                memory_in_use: 0,
                mirror_memory: 0,
                seed_memory: Vec::new(),
                shell_memory: Vec::new(),
                slice_cpus: None,
                host: None,
                cpu: None,
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
    App::new(live, None, Vec::new(), Vec::new())
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
        reserved: 9 << 30,
        warm: false,
    }
}

fn event(id: Option<u64>, event: Event) -> Msg {
    Msg::Event(EventEnvelope { id, at: NOW, event })
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::from(code))
}

/// The jobs `effects` open.
fn opened(effects: &[Effect]) -> Vec<u64> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::OpenJob(id) => Some(*id),
            _ => None,
        })
        .collect()
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
    assert_eq!(opened(&app.update(key(KeyCode::Enter))), [42]);
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
        "Waiting for a runner 4s",
        "Runner starting 5s",
        "capped at 9G and 8 CPUs",
        "##[group]Unit tests",
    ] {
        assert!(job.contains(expected), "{expected}: {job}");
    }

    let mut finished = summary(42, JobState::Completed);
    finished.conclusion = Some("success".to_owned());
    finished.timeline.finished_at = Some(1095);
    // The finished job is read again, for its steps' last state and its totals.
    assert!(
        app.update(event(Some(7), Event::JobFinished { job: finished }))
            .contains(&Effect::RefreshJob(42))
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
fn the_failed_figure_is_red_only_for_failures_of_the_runner() {
    // The colour of the count's cell in "2 failed" on the health line.
    let colour_of_failed = |runner_failed| {
        let mut app = app();
        app.activity = Some(runnerd_core::api::Activity {
            runner_failed,
            ..day()
        });
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, NOW, Theme { colour: true }))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row: Vec<&str> = (0..160).map(|x| buffer[(x, 1)].symbol()).collect();
        let x = row
            .windows(8)
            .position(|cells| cells.concat() == "2 failed")
            .unwrap();
        buffer[(u16::try_from(x).unwrap(), 1)].fg
    };
    assert_ne!(
        colour_of_failed(0),
        Color::LightRed,
        "failing tests are the repository's"
    );
    assert_eq!(
        colour_of_failed(1),
        Color::LightRed,
        "a job lost with its runner"
    );
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

fn records(jobs: &[u64]) -> Records {
    Records {
        records: jobs
            .iter()
            .map(|job| Record {
                job: *job,
                repository: "halyard/ledger".to_owned(),
                profile: "jvm".to_owned(),
                workflow: Some("build".to_owned()),
                name: Some("test".to_owned()),
                created_at: NOW - 60,
                bytes: 300 << 20,
                image: format!("runnerd-record:{job}"),
            })
            .collect(),
        limits: RecordLimits {
            max_total: 20 << 30,
            max_count: 10,
            max_age: 7 * 24 * 3600,
            min_free: 10 << 30,
        },
        shells: vec![43],
    }
}

#[test]
fn records_near_max_total_say_so_first_on_a_narrow_heading() {
    let mut app = app();
    app.update(key(KeyCode::Char('3')));
    let mut near = records(&[42, 43]);
    near.limits.max_total = 700 << 20;
    app.update(Msg::Records(near));
    let shown = screen(&app, 80, 24, false);
    assert!(
        shown.contains("near max_total, so new ones remove the oldest"),
        "{shown}"
    );
}

#[test]
fn a_record_is_removed_only_after_y_and_the_list_follows_new_ones() {
    let mut app = app();
    assert_eq!(app.update(key(KeyCode::Char('3'))), vec![Effect::Records]);
    app.update(Msg::Records(records(&[42, 43])));
    app.update(key(KeyCode::Char('j')));
    let listed = screen(&app, 80, 24, false);
    for expected in [
        "halyard/ledger",
        "build / test",
        "300M",
        "shell open",
        "runnerctl shell 43",
    ] {
        assert!(listed.contains(expected), "{expected}: {listed}");
    }

    assert_eq!(app.update(key(KeyCode::Char('d'))), Vec::new());
    assert!(
        screen(&app, 120, 24, false).contains("y removes the record of job 43"),
        "d asks first"
    );
    assert_eq!(app.update(key(KeyCode::Char('n'))), Vec::new());
    assert_eq!(app.removal, None);
    app.update(key(KeyCode::Char('d')));
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::RemoveRecords(Removal::Job(43))]
    );
    app.update(key(KeyCode::Char('D')));
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::RemoveRecords(Removal::All)]
    );

    let created = Event::RecordCreated {
        record: records(&[44]).records.remove(0),
    };
    assert_eq!(app.update(event(Some(9), created)), vec![Effect::Records]);
}

/// jvm keeps its 9G; node's builds peak at 2.1G, so 3.25G caps, with 1G of page cache room, fit 4 of node's 2 × 7G.
fn sizing() -> Sizing {
    let spread = |p50, p95, p99, max| Spread { p50, p95, p99, max };
    let advised = |name: &str, memory: u64, peak: u64, advice| {
        let peaks = Peaks {
            jobs: 40,
            at_cap: 0,
            memory: spread(peak / 2, peak, peak, peak),
            cpu: spread(2000, 6000, 7000, 7000),
            cpu_runs: 40,
            on_cap: 0,
        };
        ProfileSizing {
            name: name.to_owned(),
            memory,
            cpus: 8,
            slots: 2,
            peaks: peaks.clone(),
            wait: spread(20, 300, 400, 400),
            slot_wait: spread(10, 240, 300, 300),
            starts: Starts {
                warm: 30,
                warm_p50: 2,
                cold: 10,
                cold_p50: 24,
            },
            busy: 70,
            wants_slots: true,
            advice,
            jobs: vec![JobSizing {
                repository: "halyard/storefront".to_owned(),
                workflow: Some("CI".to_owned()),
                name: "build".to_owned(),
                peaks,
                wait: spread(20, 300, 400, 400),
                advice,
            }],
            revert: None,
            hold: None,
        }
    };
    Sizing {
        auto_plan: None,
        auto: false,
        changes: Vec::new(),
        since: NOW - 600,
        until: NOW,
        margin: 15,
        sample_interval: 2,
        min_jobs: 10,
        memory_max: Some(33 << 30),
        admission: AdmissionPolicy::Caps,
        profiles: vec![
            advised("jvm", 9 << 30, 7800 << 20, Advice::Keep { memory: 9 << 30 }),
            advised(
                "node",
                7 << 30,
                2150 << 20,
                Advice::Lower {
                    memory: 3328 << 20,
                    slots: 4,
                },
            ),
        ],
    }
}

#[test]
fn sizing_advice_is_written_into_the_config_only_after_y() {
    let mut app = app();
    assert_eq!(app.update(key(KeyCode::Char('4'))), vec![Effect::Sizing]);
    app.update(Msg::Sizing(sizing()));
    assert_eq!(
        app.update(key(KeyCode::Char('a'))),
        Vec::new(),
        "jvm has nothing to apply"
    );
    assert_eq!(app.resize, None);
    app.update(key(KeyCode::Char('j')));
    let shown = screen(&app, 120, 24, false);
    for expected in [
        "Sizing",
        "node: lower memory to 3.25G from 7G, its largest peak + 1G for page cache; 4 slots fit in the same 14G",
        "halyard/storefront",
        "CI / build",
        "a writes memory = \"3.25G\", slots = 4",
    ] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }

    app.update(key(KeyCode::Char('a')));
    assert!(
        screen(&app, 120, 24, false).contains("y writes memory = \"3.25G\", slots = 4 for node"),
        "a asks first"
    );
    assert_eq!(app.update(key(KeyCode::Char('n'))), Vec::new());
    app.update(key(KeyCode::Char('a')));
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::Resize(Resizing {
            profile: "node".to_owned(),
            resize: Resize {
                memory: 3328 << 20,
                slots: 4,
                cpus: None
            }
        })]
    );
    // A finished job changes the percentiles: the view reads them again.
    let finished = Event::JobFinished {
        job: summary(42, JobState::Completed),
    };
    assert!(
        app.update(event(Some(9), finished))
            .contains(&Effect::Sizing)
    );
    let narrow = screen(&app, 80, 24, false);
    assert!(
        narrow.contains("WAIT P95") && narrow.contains("same 14G"),
        "the sizing view fits 80 columns, the advice on two lines: {narrow}"
    );
    let wide = screen(&app, 140, 24, false);
    assert!(
        wide.contains("P50 WARM/COLD") && wide.contains("2s / 24s"),
        "wide, the waits of warm and cold starts show: {wide}"
    );
}

fn queued(id: u64, repository: &str, name: &str, at: u64) -> QueuedJob {
    QueuedJob {
        id,
        repository: repository.to_owned(),
        profile: None,
        run_id: 4002,
        run_attempt: 1,
        workflow: Some("E2E".to_owned()),
        name: name.to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        queued_at: at,
        waiting: None,
    }
}

#[test]
fn the_queue_lists_each_waiting_job_oldest_first_with_why_and_opens_and_pauses_like_the_others() {
    let mut app = app();
    app.live.status.queued = vec![
        QueuedJob {
            waiting: Some(Waiting::NoSlot {
                profile: "node".to_owned(),
            }),
            ..queued(51, "halyard/storefront", "visual-diff", 1041)
        },
        QueuedJob {
            waiting: Some(Waiting::Paused),
            ..queued(52, "halyard/storefront", "a11y", 1090)
        },
    ];
    app.live.recent = vec![summary(42, JobState::Completed)];
    for width in [80, 120] {
        let shown = screen(&app, width, 24, false);
        let rows = rows_with(&shown, "E2E / ");
        assert_eq!(rows.len(), 2, "{shown}");
        assert!(
            rows[0].trim_start().starts_with("59s E2E / visual-diff")
                && rows[0].trim_end().ends_with("The node slot is busy"),
            "the oldest first, with how long it waited and why: {rows:?}"
        );
        assert!(
            rows[1].trim_end().ends_with("halyard/storefront is paused"),
            "{rows:?}"
        );
        assert!(
            shown.contains("2 waiting, oldest 59s"),
            "the health line counts them: {shown}"
        );
    }

    app.update(key(KeyCode::Tab));
    assert_eq!(
        app.update(key(KeyCode::Char('p'))),
        vec![Effect::Pause {
            repository: Some("halyard/storefront".to_owned()),
            force: false
        }]
    );
    assert_eq!(opened(&app.update(key(KeyCode::Enter))), [51]);
    let job = screen(&app, 120, 40, false);
    let head: Vec<&str> = job.lines().skip(1).take(2).map(str::trim_end).collect();
    assert_eq!(
        head,
        ["■ halyard/storefront", "E2E / visual-diff  QUEUED"],
        "{job}"
    );
    assert!(
        job.contains("Waiting for a runner 59s…  The node slot is busy"),
        "its page says why it waits: {job}"
    );
    assert!(
        !job.contains("Memory") && !job.contains("Steps"),
        "nothing to chart or list before a runner has it: {job}"
    );
}

#[test]
fn a_queued_job_left_open_shows_what_its_runner_reserved_once_it_starts() {
    let mut app = app();
    app.live.status.queued = vec![queued(51, "halyard/ledger", "build", 1000)];
    app.update(key(KeyCode::Tab));
    assert_eq!(opened(&app.update(key(KeyCode::Enter))), [51]);
    let effects = app.update(event(
        Some(9),
        Event::JobStarted {
            job: summary(51, JobState::Running),
        },
    ));
    assert!(
        effects.contains(&Effect::RefreshJob(51)),
        "the started job is read again for its admission: {effects:?}"
    );
    let summary = app.job.as_ref().unwrap().job.summary.clone();
    app.update(Msg::Job(Job {
        admission: Some(measured()),
        summary,
        steps: vec![],
        record: None,
    }));
    let job = screen(&app, 120, 40, false);
    for expected in ["reserved 6.5G", "reserved 3.6 CPUs"] {
        assert!(job.contains(expected), "{expected}: {job}");
    }
}

#[test]
fn the_selected_job_stays_selected_when_jobs_above_it_start_or_finish() {
    let mut app = app();
    app.live.status.queued = vec![
        queued(51, "halyard/ledger", "build", 1000),
        queued(52, "halyard/storefront", "build", 1000),
    ];
    app.live.recent = vec![summary(42, JobState::Completed)];
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
    assert_eq!(opened(&app.update(key(KeyCode::Enter))), [52]);
    app.update(key(KeyCode::Esc));

    // The last queued job starts: the queue is gone, and so is its selection.
    app.update(event(
        Some(10),
        Event::JobStarted {
            job: summary(52, JobState::Running),
        },
    ));
    assert_eq!(app.pane, Pane::Slots, "an empty queue keeps no selection");
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.pane, Pane::Latest, "tab passes the empty queue");
    let mut newer = summary(43, JobState::Completed);
    newer.timeline.finished_at = Some(1090);
    app.update(event(Some(11), Event::JobFinished { job: newer }));
    assert_eq!(
        opened(&app.update(key(KeyCode::Enter))),
        [42],
        "a job finishing above the selected one leaves it selected"
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
    let names = shown.lines().nth(4).unwrap();
    assert!(
        names.contains("mirror 4G") && names.contains("jvm seed 9G"),
        "{shown}"
    );

    // A 512M mirror is a cell or two at common widths: the heading names it.
    app.live.status.budget.mirror_memory = 512 << 20;
    let shown = screen(&app, 120, 40, false);
    let mut lines = shown.lines().skip(2);
    let (heading, bar) = (lines.next().unwrap(), lines.next().unwrap());
    assert!(heading.ends_with("· mirror 512M"), "{shown}");
    assert!(bar.contains("▒ "), "the mirror's block shows: {shown}");

    // Without a MemoryMax the bar's whole is every block together, so the seed container still has its block.
    app.live.status.budget.memory_max = None;
    let shown = screen(&app, 120, 40, false);
    let names = shown.lines().nth(4).unwrap();
    assert!(names.contains("jvm seed 9G"), "{shown}");
}

#[test]
fn the_bar_shows_no_free_cell_when_core_says_nothing_is_free() {
    // Nothing runs, and the host leaves the slice 4G: it holds all 8G past the three slots' 25G.
    let mut app = app();
    app.live.status.budget.host = Some(HostBudget {
        memory: HostMemory {
            at: NOW,
            total: 64 << 30,
            available: 8 << 30,
            slice: 0,
            some: None,
            full: None,
            holders: Vec::new(),
            ceiling: None,
        },
        limit: 4 << 30,
        spare: 4 << 30,
        stalled: false,
    });
    assert_eq!(app.live.status.budget_parts().free, None);
    let shown = screen(&app, 80, 24, false);
    let bar = shown.lines().nth(3).unwrap();
    assert!(!bar.contains('·'), "no free cell: {shown}");
    assert!(bar.ends_with('╱'), "the host's part ends the bar: {shown}");

    // Held under measured admission, nothing runs and all 33G is closed.
    let budget = &mut app.live.status.budget;
    budget.admission = AdmissionPolicy::Measured;
    budget.held = true;
    let shown = screen(&app, 80, 24, false);
    let mut lines = shown.lines().skip(3);
    let (bar, names) = (lines.next().unwrap(), lines.next().unwrap());
    assert!(!bar.contains('·'), "no free cell while held: {shown}");
    assert!(names.contains("closed past 90% 33G"), "{shown}");
}

#[test]
fn a_budget_label_never_cuts_a_figure_at_any_width() {
    let mut app = app();
    running_job(&mut app);
    app.update(event(
        None,
        Event::JobSample {
            job: 42,
            sample: Sample {
                at: 1096,
                cpu: 3140,
                memory: 3328 << 20,
                network_received: 0,
                network_sent: 0,
            },
        },
    ));
    for width in 60..=160 {
        let shown = screen(&app, width, 40, false);
        let names = shown.lines().nth(4).unwrap();
        assert!(
            !names.contains('…'),
            "at {width} columns a label is cut: {names}"
        );
    }
}

#[test]
fn the_budget_names_a_shell_open_in_a_record() {
    let mut app = app();
    let budget = &mut app.live.status.budget;
    budget.shell_memory = vec![ShellMemory {
        job: 42,
        memory: 9 << 30,
    }];
    budget.memory_in_use += 9 << 30;
    let shown = screen(&app, 160, 40, false);
    let names = shown.lines().nth(4).unwrap();
    assert!(names.contains("job 42 shell 9G"), "{shown}");
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
                    admission: AdmissionPolicy::Caps,
                    held: false,
                    slice: "ci.slice".to_owned(),
                    memory_max: None,
                    memory_in_use: in_use,
                    mirror_memory: 0,
                    seed_memory: Vec::new(),
                    shell_memory: Vec::new(),
                    slice_cpus: None,
                    host: None,
                    cpu: None,
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
    // Past the empty queue and the repositories.
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    assert_eq!(opened(&app.update(key(KeyCode::Enter))), [41]);
    app.update(Msg::LogLine {
        job: 99,
        text: "another job's line".to_owned(),
    });
    app.update(Msg::LogLine {
        job: 41,
        text: "##[error]Process completed with exit code 1.".to_owned(),
    });
    app.update(Msg::Job(Job {
        admission: None,
        summary: app.job.as_ref().unwrap().job.summary.clone(),
        steps: vec![step(1, "Set up job", 1009, Some(1012))],
        record: None,
    }));
    let job = screen(&app, 120, 40, false);
    assert!(job.contains("FAILURE"), "{job}");
    assert!(job.contains("exit code 1"), "{job}");
    assert!(!job.contains("another job's line"), "{job}");
}

/// A runner admitted by measured peaks: 6.5G and 3.6 CPUs reserved.
fn measured() -> Admission {
    Admission {
        policy: AdmissionPolicy::Measured,
        reserved: 6656 << 20,
        basis: Basis::Cap,
        free: None,
        slice: None,
        cpu: Some(CpuAdmission {
            reserved: 3600,
            basis: Basis::Cap,
            free: 38_700,
            waited: None,
        }),
    }
}

#[test]
fn an_open_jobs_charts_say_what_it_reserved_beside_its_caps() {
    let mut app = app();
    running_job(&mut app);
    app.update(key(KeyCode::Enter));
    let measured = measured();
    let caps = Admission {
        policy: AdmissionPolicy::Caps,
        reserved: 9 << 30,
        cpu: None,
        ..measured
    };
    let cases: [(_, &[&str], &str); 3] = [
        (
            Some(measured),
            &["cap 9G", "reserved 6.5G", "cap 8 CPUs", "reserved 3.6 CPUs"],
            "cap and reserved",
        ),
        // The reservation is the cap's, not a line of its own.
        (
            Some(caps),
            &["cap and reserved 9G", "cap 8 CPUs"],
            "─ reserved",
        ),
        (None, &["cap 9G", "cap 8 CPUs"], "reserved"),
    ];
    for (admission, expected, unexpected) in cases {
        let summary = app.job.as_ref().unwrap().job.summary.clone();
        app.update(Msg::Job(Job {
            admission,
            summary,
            steps: vec![],
            record: None,
        }));
        let job = screen(&app, 120, 40, false);
        for expected in expected {
            assert!(job.contains(expected), "{expected}: {job}");
        }
        assert!(!job.contains(unexpected), "no {unexpected:?}: {job}");
    }
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
        admission: None,
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
            overview.contains("just now"),
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
        ..JobMetrics::default()
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

#[test]
fn the_help_shows_every_key_in_an_80_by_24_terminal() {
    let mut app = app();
    app.update(key(KeyCode::Char('?')));
    let shown = screen(&app, 80, 24, false);
    for expected in ["overview", "write the advice", "this help", "suspend"] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }
}

#[test]
fn an_empty_records_view_says_in_full_how_records_are_kept() {
    let mut app = app();
    app.update(key(KeyCode::Char('3')));
    app.update(Msg::Records(records(&[])));
    let shown: String = screen(&app, 80, 24, false)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        shown.contains("as a record, to open a shell where it failed"),
        "the hint wraps instead of being cut: {shown}"
    );
}

/// A day in which jobs that waited 10 s, 20 s and 300 s started, and 2 of 30 finished jobs failed.
fn day() -> runnerd_core::api::Activity {
    let started = |at, waited| runnerd_core::api::Started { at, waited };
    runnerd_core::api::Activity {
        since: 0,
        until: NOW,
        started: vec![started(200, 10), started(600, 20), started(900, 300)],
        finished: 30,
        failed: 2,
        runner_failed: 0,
    }
}

#[test]
fn the_overview_opens_with_ci_health_and_keeps_its_figures_whole_at_80_columns() {
    let mut app = app();
    app.activity = Some(day());
    app.live.status.queued = vec![queued(51, "halyard/storefront", "visual-diff", 1041)];
    let wide = screen(&app, 160, 40, false);
    let health = wide.lines().nth(1).unwrap();
    for expected in [
        "0/3 running",
        "1 waiting, oldest 59s",
        "24 h: wait p95 5m 00s",
        "2 failed of 30 finished",
    ] {
        assert!(health.contains(expected), "{expected}: {health}");
    }
    assert!(health.contains('█'), "the longest hour's bar: {health}");

    let small = screen(&app, 80, 24, false);
    let health = small.lines().nth(1).unwrap();
    assert!(
        health.contains("24 h: wait p95 5m 00s") && health.contains("2 failed"),
        "at 80 columns the figures stay and the details go whole: {health}"
    );
    assert!(!health.contains('…'), "nothing cut mid-figure: {health}");
}

#[test]
fn the_health_line_counts_the_warning_insights_and_says_where_to_read_them() {
    use runnerd_core::api::{Insight, InsightKind, Insights, Severity};
    let insight = |key: &str, severity| Insight {
        key: key.to_owned(),
        severity,
        repository: "halyard/api".to_owned(),
        workflow: Some("CI".to_owned()),
        job: Some("test".to_owned()),
        sentence: "halyard/api: CI / test failed and then passed on a re-run in 1 of its 3 runs."
            .to_owned(),
        kind: InsightKind::Flaky { flaky: 1, total: 3 },
        runs: Vec::new(),
    };
    let mut app = app();
    app.activity = Some(day());
    app.update(Msg::Insights(Box::new(Insights {
        since: 0,
        until: NOW,
        insights: vec![
            insight("flaky:a", Severity::Warning),
            insight("flaky:b", Severity::Warning),
            insight("waits:halyard/api", Severity::Notice),
        ],
        waits: Vec::new(),
    })));

    let wide = screen(&app, 160, 40, false);
    let health = wide.lines().nth(1).unwrap();
    assert!(
        health.contains("2 insights: runnerctl insights"),
        "notices are not counted: {health}"
    );
    let small = screen(&app, 80, 24, false);
    let health = small.lines().nth(1).unwrap();
    assert!(
        !health.contains("runnerctl insights") && health.contains("2 failed"),
        "the pointer goes first at 80 columns: {health}"
    );
}

#[test]
fn the_day_s_figures_are_read_again_as_jobs_start_and_end_and_once_a_minute_otherwise() {
    let mut app = app();
    app.update(Msg::Activity {
        generation: 1,
        activity: day(),
    });
    let host = |at| {
        event(
            None,
            Event::HostSample {
                sample: runnerd_core::api::HostSample {
                    at,
                    cpu: 0,
                    memory: 0,
                },
            },
        )
    };
    assert!(
        !app.update(host(NOW + 30)).iter().any(asks_activity),
        "read a moment ago"
    );
    assert!(app.update(host(NOW + 60)).iter().any(asks_activity));
    assert!(
        !app.update(host(NOW + 62)).iter().any(asks_activity),
        "asked once, not with every sample while the answer comes"
    );
    assert!(
        app.update(event(
            Some(1),
            Event::JobStarted {
                job: summary(42, JobState::Running)
            }
        ))
        .iter()
        .any(asks_activity),
        "a start changes the waits"
    );
}

#[test]
fn the_history_filters_by_the_workflows_its_repository_has_run() {
    let mut app = app();
    let (generation, _) = history_request(&app.update(key(KeyCode::Char('2'))));
    let mut e2e = summary(43, JobState::Completed);
    e2e.repository = "halyard/storefront".to_owned();
    e2e.workflow = Some("E2E".to_owned());
    app.update(history_page(
        generation,
        vec![summary(42, JobState::Completed), e2e],
        None,
    ));
    let ci = history_query(&app.update(key(KeyCode::Char('w'))));
    assert_eq!(
        ci.workflow.as_deref(),
        Some("CI"),
        "by name, across repositories"
    );
    let e2e = history_query(&app.update(key(KeyCode::Char('w'))));
    assert_eq!(e2e.workflow.as_deref(), Some("E2E"));
    assert!(screen(&app, 120, 24, false).contains("workflow E2E"));
    let any = history_query(&app.update(key(KeyCode::Char('w'))));
    assert_eq!(any.workflow, None, "then any again");

    app.update(key(KeyCode::Char('w')));
    let ledger = history_query(&app.update(key(KeyCode::Char('s'))));
    assert_eq!(
        (ledger.repository.as_deref(), ledger.workflow.as_deref()),
        (Some("halyard/ledger"), Some("CI")),
        "a workflow the repository runs stays"
    );
    let storefront = history_query(&app.update(key(KeyCode::Char('s'))));
    assert_eq!(
        (storefront.repository.as_deref(), storefront.workflow),
        (Some("halyard/storefront"), None),
        "one it never ran would show nothing, so it goes"
    );
    let only = history_query(&app.update(key(KeyCode::Char('w'))));
    assert_eq!(
        only.workflow.as_deref(),
        Some("E2E"),
        "only its own workflows"
    );
}

#[test]
fn a_job_s_page_compares_it_with_the_same_job_s_latest_runs() {
    let mut app = app();
    running_job(&mut app);
    let effects = app.update(key(KeyCode::Enter));
    let Some(Effect::Runs { job, query }) = effects
        .iter()
        .find(|effect| matches!(effect, Effect::Runs { .. }))
        .cloned()
    else {
        panic!("asks for its runs: {effects:?}");
    };
    assert_eq!(
        (
            query.repository.as_deref(),
            query.workflow.as_deref(),
            query.name.as_deref()
        ),
        (Some("halyard/ledger"), Some("CI"), Some("build"))
    );
    let run = |id, ran: u64, conclusion: &str| {
        let mut run = finished(id, 100);
        run.timeline.queued_at = 0;
        run.timeline.started_at = Some(10);
        run.timeline.finished_at = Some(10 + ran);
        run.conclusion = Some(conclusion.to_owned());
        run.metrics.memory_peak = Some(6 << 30);
        run
    };
    let mut other_workflow = run(30, 999, "failure");
    other_workflow.workflow = Some("Nightly".to_owned());
    let mut newest_first = vec![
        summary(42, JobState::Running),
        run(33, 300, "success"),
        run(32, 100, "failure"),
        run(31, 200, "success"),
        other_workflow,
    ];
    app.update(Msg::Runs {
        job,
        page: Ok(runnerd_core::api::JobPage {
            jobs: std::mem::take(&mut newest_first),
            next: None,
        }),
    });
    let shown = screen(&app, 120, 40, false);
    let heading = rows_with(&shown, "Latest 4 runs");
    assert_eq!(
        heading.len(),
        1,
        "another workflow's job of that name is left out: {shown}"
    );
    for expected in [
        "median run 3m 20s",
        "wait 10s",
        "1 of 3 failed",
        "largest peak 6G",
    ] {
        assert!(heading[0].contains(expected), "{expected}: {shown}");
    }
    let bars = rows_with(&shown, "  waited ")[0];
    assert!(
        bars.starts_with("ran ") && bars.contains("peak "),
        "{shown}"
    );
    assert!(
        screen(&app, 80, 24, false).contains("Latest 4 runs"),
        "it stays at 80 by 24"
    );
}

#[test]
fn sizing_says_what_auto_sizing_changes_next_and_when_or_why_it_will_not() {
    use runnerd_core::api::{AutoPlan, SizingChange, SizingHold};
    let mut app = app();
    app.update(key(KeyCode::Char('4')));
    let mut sizing = sizing();
    sizing.auto = true;
    sizing.auto_plan = Some(AutoPlan {
        every: 300,
        interval: 86_400,
        raise_interval: 3600,
        max_step: 25,
        next: vec![SizingChange {
            at: NOW,
            profile: "node".to_owned(),
            from: Resize {
                memory: 7 << 30,
                slots: 2,
                cpus: None,
            },
            to: Resize {
                memory: 5376 << 20,
                slots: 2,
                cpus: None,
            },
            reason: "lower memory".to_owned(),
            uncertain: false,
        }],
        held: vec![SizingHold {
            profile: "jvm".to_owned(),
            changed: NOW - 600,
            until: NOW + 7200,
            lower_until: NOW + 86_400 - 600,
        }],
        blocked: None,
        refused: Vec::new(),
        refused_slots: Vec::new(),
    });
    app.update(Msg::Sizing(sizing.clone()));
    for width in [80, 120] {
        let shown = screen(&app, width, 24, false);
        assert!(
            shown.contains("looks every 5 minutes") && shown.contains("25% at a time"),
            "the rules, wrapped rather than cut: {shown}"
        );
        let jvm = rows_with(&shown, "■ jvm")[0];
        assert!(jvm.contains("No change for 2h 00m"), "{shown}");
        let node = rows_with(&shown, "■ node")[0];
        assert!(
            node.contains("Next look, within 5 minutes: memory 7G → 5.25G, slots 2"),
            "{shown}"
        );
    }

    let heading = rows_with(&screen(&app, 120, 24, false), "of history")[0].to_owned();
    assert!(
        heading.contains("peaks sampled every 2 s"),
        "the sampling caveat survives a 120-column heading: {heading}"
    );

    sizing.auto_plan.as_mut().unwrap().blocked =
        Some("runnerd.toml changed on disk since runnerd read it".to_owned());
    app.update(Msg::Sizing(sizing.clone()));
    let shown = screen(&app, 120, 24, false);
    let blocked = sizing.auto_plan.as_ref().unwrap().blocked_line().unwrap();
    assert!(shown.contains(&blocked), "{shown}");
    assert!(
        rows_with(&shown, "■ node")[0].contains("Once that is fixed: memory 7G → 5.25G"),
        "{shown}"
    );

    // Refused CPU and slot raises show beside the memory change the same look makes, here and in runnerctl sizing.
    let plan = runnerd_fixtures::auto_plan(NOW);
    let refusals = plan.refusals("node");
    assert_eq!(
        refusals.len(),
        2,
        "a CPU and a slot raise refused: {refusals:?}"
    );
    sizing.auto_plan = Some(plan);
    app.update(Msg::Sizing(sizing.clone()));
    let shown = screen(&app, 120, 40, false);
    assert!(
        rows_with(&shown, "■ node")[0].contains("Next look, within 5 minutes: memory 7G → 5.25G"),
        "{shown}"
    );
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let printed = crate::human::sizing(&sizing, NOW, crate::human::Paint { colour: false });
    for refusal in &refusals {
        assert!(
            words(&shown).contains(&words(refusal)),
            "the refusal shows beside it, wrapped, never cut: {shown}"
        );
        assert!(printed.contains(refusal), "{printed}");
    }
}

/// Whether an effect reads the day's activity.
fn asks_activity(effect: &Effect) -> bool {
    matches!(effect, Effect::Activity { .. })
}

/// The generation of the activity read `effects` ask for.
fn activity_generation(effects: &[Effect]) -> u64 {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Activity { generation } => Some(*generation),
            _ => None,
        })
        .expect("an activity read")
}

#[test]
fn an_activity_read_that_answers_after_a_newer_one_is_ignored() {
    let mut app = app();
    let started = activity_generation(&app.update(event(
        Some(1),
        Event::JobStarted {
            job: summary(42, JobState::Running),
        },
    )));
    let mut failed = summary(42, JobState::Completed);
    failed.conclusion = Some("failure".to_owned());
    failed.timeline.finished_at = Some(1090);
    let finished =
        activity_generation(&app.update(event(Some(2), Event::JobFinished { job: failed })));
    let mut after = day();
    after.failed = 3;
    after.finished = 31;
    app.update(Msg::Activity {
        generation: finished,
        activity: after,
    });
    app.update(Msg::Activity {
        generation: started,
        activity: day(),
    });
    let health = screen(&app, 160, 40, false);
    assert!(
        health.contains("3 failed of 31 finished"),
        "the older read does not take the failure back: {health}"
    );
}

#[test]
fn a_frame_without_columns_or_rows_draws_nothing_and_does_not_panic() {
    let mut app = app();
    running_job(&mut app);
    app.activity = Some(day());
    app.live.status.queued = vec![queued(51, "halyard/storefront", "visual-diff", 1041)];
    for view in ['1', '2', '3', '4'] {
        app.update(key(KeyCode::Char(view)));
        for (width, height) in [(0, 24), (80, 0), (0, 0), (1, 1)] {
            screen(&app, width, height, true);
        }
    }
    app.update(key(KeyCode::Char('1')));
    app.update(key(KeyCode::Enter));
    for (width, height) in [(0, 24), (80, 0), (0, 0), (1, 1)] {
        screen(&app, width, height, true);
    }
}

#[test]
fn a_reason_read_before_a_job_or_slot_changed_is_not_shown() {
    let mut app = app();
    let mut a = queued(51, "halyard/ledger", "build", 1000);
    a.workflow = Some("CI".to_owned());
    let mut b = queued(52, "halyard/ledger", "test", 1001);
    b.waiting = Some(Waiting::BehindStart { job: 51 });
    app.live.status.queued = vec![a, b];
    let before = app.live.status.clone();
    let shown = screen(&app, 120, 24, false);
    assert!(
        shown.contains("Next after build, whose runner starts first"),
        "{shown}"
    );

    // A periodic read is under way when A starts.
    let host = event(
        None,
        Event::HostSample {
            sample: runnerd_core::api::HostSample {
                at: NOW,
                cpu: 0,
                memory: 0,
            },
        },
    );
    assert!(app.update(host).contains(&Effect::Reasons));
    app.update(event(
        Some(9),
        Event::JobStarted {
            job: summary(51, JobState::Running),
        },
    ));
    let shown = screen(&app, 120, 24, false);
    assert!(
        !shown.contains("Next after"),
        "a start may change every reason: none shows until read again: {shown}"
    );
    assert_eq!(
        app.update(Msg::Reasons(Some(Box::new(before)))),
        vec![Effect::Reasons],
        "the read from before the start is dropped and another made"
    );
    let shown = screen(&app, 120, 24, false);
    assert!(!shown.contains("Next after"), "{shown}");
    assert!(
        app.update(Msg::Reasons(None)).is_empty(),
        "a read after the start that fails asks for nothing until the next sample or change"
    );
    let shown = screen(&app, 120, 24, false);
    assert!(
        !shown.contains("Next after"),
        "a failed read leaves no reason rather than the old one: {shown}"
    );
}

#[test]
fn a_queued_job_s_reason_is_read_again_one_read_at_a_time_since_no_event_carries_it() {
    let mut app = app();
    let queued_event = |id| {
        event(
            Some(id),
            Event::JobQueued {
                job: summary(id, JobState::Queued),
            },
        )
    };
    assert_eq!(app.update(queued_event(51)), vec![Effect::Reasons]);
    assert!(
        app.update(queued_event(52)).is_empty(),
        "a burst makes one read at a time"
    );
    assert_eq!(
        app.update(Msg::Reasons(None)),
        vec![Effect::Reasons],
        "the second job queued during the read: read again"
    );
    let mut read = app.live.status.clone();
    read.queued[0].waiting = Some(Waiting::NoSlot {
        profile: "jvm".to_owned(),
    });
    // The read only knows the first job; the queue keeps the second from its event.
    read.queued.truncate(1);
    assert!(
        app.update(Msg::Reasons(Some(Box::new(read)))).is_empty(),
        "nothing changed during this read"
    );
    assert_eq!(
        app.live.status.queued.len(),
        2,
        "{:?}",
        app.live.status.queued
    );
    let shown = screen(&app, 120, 24, false);
    assert!(shown.contains("All 2 jvm slots are busy"), "{shown}");

    let host = |at| {
        event(
            None,
            Event::HostSample {
                sample: runnerd_core::api::HostSample {
                    at,
                    cpu: 0,
                    memory: 0,
                },
            },
        )
    };
    assert!(app.update(host(NOW)).contains(&Effect::Reasons));
    app.update(Msg::Reasons(None));
    assert!(
        !app.update(host(NOW + 5)).contains(&Effect::Reasons),
        "read again every 10 s while nothing changes"
    );
    assert!(app.update(host(NOW + 10)).contains(&Effect::Reasons));
}

#[test]
fn the_open_job_s_own_bar_among_its_runs_follows_it_through_start_and_a_missed_finish() {
    let mut app = app();
    let mut waiting = queued(42, "halyard/ledger", "build", 1000);
    waiting.workflow = Some("CI".to_owned());
    app.live.status.queued = vec![waiting];
    app.update(key(KeyCode::Tab));
    let effects = app.update(key(KeyCode::Enter));
    let Some(Effect::Runs { job, .. }) = effects
        .iter()
        .find(|effect| matches!(effect, Effect::Runs { .. }))
        .cloned()
    else {
        panic!("{effects:?}");
    };
    let earlier = |id| {
        let mut run = finished(id, 500);
        run.timeline.queued_at = 0;
        run.timeline.started_at = Some(10);
        run.timeline.finished_at = Some(110);
        run
    };
    app.update(Msg::Runs {
        job,
        page: Ok(runnerd_core::api::JobPage {
            jobs: vec![summary(42, JobState::Queued), earlier(32), earlier(31)],
            next: None,
        }),
    });
    // The newest run's bar is the last before the waits.
    let ran_bar = |app: &App| {
        let shown = screen(app, 120, 40, false);
        let row = rows_with(&shown, "  waited ")[0].to_owned();
        let before = row.split("  waited ").next().unwrap().to_owned();
        (before.chars().last().unwrap(), shown)
    };
    assert_eq!(ran_bar(&app).0, ' ', "queued, it has not run");

    app.update(event(
        Some(1),
        Event::JobStarted {
            job: summary(42, JobState::Running),
        },
    ));
    let (bar, shown) = ran_bar(&app);
    assert_ne!(bar, ' ', "started, it shows its run so far: {shown}");

    app.update(Msg::Disconnected("runnerd is not running".to_owned()));
    let effects = app.update(Msg::Reconnected);
    assert!(effects.contains(&Effect::RefreshJob(42)), "{effects:?}");
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Runs { job: 42, .. })),
        "its runs are read again too: {effects:?}"
    );
    let mut done = summary(42, JobState::Completed);
    done.conclusion = Some("failure".to_owned());
    done.timeline.finished_at = Some(1090);
    app.update(Msg::Job(Job {
        admission: None,
        summary: done,
        steps: Vec::new(),
        record: None,
    }));
    let shown = screen(&app, 120, 40, false);
    assert!(
        rows_with(&shown, "Latest 3 runs")[0].contains("1 of 3 failed"),
        "the finish read again counts among its runs: {shown}"
    );
}

#[test]
fn the_open_job_s_peak_among_its_runs_rises_with_its_samples_until_its_record_is_final() {
    let mut app = app();
    running_job(&mut app);
    // Opened after its record's peak was 2G, with no samples seen here yet.
    app.series.clear();
    app.live
        .running
        .get_mut(&42)
        .unwrap()
        .summary
        .metrics
        .memory_peak = Some(2 << 30);
    let effects = app.update(key(KeyCode::Enter));
    let Some(Effect::Runs { job, .. }) = effects
        .iter()
        .find(|effect| matches!(effect, Effect::Runs { .. }))
        .cloned()
    else {
        panic!("{effects:?}");
    };
    let mut current = summary(42, JobState::Running);
    current.metrics.memory_peak = Some(2 << 30);
    let mut earlier = finished(31, 500);
    earlier.metrics.memory_peak = Some(6 << 30);
    app.update(Msg::Runs {
        job,
        page: Ok(runnerd_core::api::JobPage {
            jobs: vec![current, earlier],
            next: None,
        }),
    });
    let sample = |memory| {
        event(
            None,
            Event::JobSample {
                job: 42,
                sample: Sample {
                    at: 1096,
                    cpu: 1000,
                    memory,
                    network_received: 0,
                    network_sent: 0,
                },
            },
        )
    };
    app.update(sample(8 << 30));
    // The peaks' bars, oldest first: the earlier run's 6G, then this one's.
    let peaks = |app: &App| {
        let shown = screen(app, 120, 40, false);
        let row = rows_with(&shown, "  peak ")[0].to_owned();
        let heading = rows_with(&shown, "Latest 2 runs")[0].to_owned();
        let bars: Vec<char> = row
            .split("  peak ")
            .nth(1)
            .unwrap()
            .trim_end()
            .chars()
            .collect();
        (heading, bars)
    };
    let (heading, bars) = peaks(&app);
    assert!(heading.contains("largest peak 8G"), "{heading}");
    assert_eq!(bars, ['▆', '█'], "this run's 8G tops the earlier 6G");

    // A lower sample, or a refresh of the record while it runs, does not lower the mark.
    app.update(sample(1 << 30));
    let mut refreshed = summary(42, JobState::Running);
    refreshed.metrics.memory_peak = Some(3 << 30);
    app.update(Msg::Job(Job {
        admission: None,
        summary: refreshed,
        steps: Vec::new(),
        record: None,
    }));
    assert!(
        peaks(&app).0.contains("largest peak 8G"),
        "{:?}",
        peaks(&app)
    );

    // The final record's totals win.
    let mut done = summary(42, JobState::Completed);
    done.conclusion = Some("success".to_owned());
    done.timeline.finished_at = Some(1098);
    done.metrics.memory_peak = Some(7 << 30);
    app.update(Msg::Job(Job {
        admission: None,
        summary: done,
        steps: Vec::new(),
        record: None,
    }));
    assert!(
        peaks(&app).0.contains("largest peak 7G"),
        "{:?}",
        peaks(&app)
    );
}

#[test]
fn a_warm_runner_reads_as_warm_not_as_a_runner_stuck_waiting_for_its_job() {
    let mut app = app();
    app.update(event(
        Some(1),
        Event::SlotChanged {
            profile: "jvm".to_owned(),
            slot: SlotStatus {
                slot: 1,
                runner: Some(RunnerStatus {
                    warm: true,
                    ..runner(false)
                }),
            },
        },
    ));
    let shown = screen(&app, 160, 40, false);
    let health = shown.lines().nth(1).unwrap();
    assert!(
        health.contains("0/3 running") && health.contains("1 warm"),
        "the warm runner runs nothing and is not starting: {health}"
    );
    assert!(!health.contains("starting"), "{health}");
    assert!(
        shown.contains("A warm runner for halyard/ledger, waiting for a job"),
        "its slot card says so: {shown}"
    );
    assert!(!shown.contains("waiting for its job"), "{shown}");
    let narrow = screen(&app, 80, 20, false);
    assert!(
        narrow.contains("warm runner for halyard/ledger, waiting for a job"),
        "its slot row says so: {narrow}"
    );
}

#[test]
fn a_step_shows_the_duration_it_has_and_none_it_lacks() {
    let mut app = app();
    running_job(&mut app);
    app.update(key(KeyCode::Enter));
    let summary = app.job.as_ref().unwrap().job.summary.clone();
    let mut cancelled = step(2, "Deploy", 1010, Some(1070));
    cancelled.conclusion = Some("cancelled".to_owned());
    app.update(Msg::Job(Job {
        admission: None,
        summary,
        steps: vec![
            Step {
                completed_at: None,
                status: "completed".to_owned(),
                ..step(1, "Lint", 1009, Some(1012))
            },
            cancelled,
        ],
        record: None,
    }));
    let job = screen(&app, 120, 40, false);
    let lint = rows_with(&job, "Lint")[0];
    assert!(
        lint.trim_end().ends_with('–') && !lint.contains("0s"),
        "a step that ended without a completion time has no duration: {job}"
    );
    let deploy = rows_with(&job, "Deploy")[0];
    assert!(
        deploy.trim_end().ends_with("1m 00s"),
        "a cancelled step keeps its duration: {job}"
    );
}

#[test]
fn a_sizing_row_of_a_job_without_a_name_says_a_job() {
    let mut app = app();
    app.update(key(KeyCode::Char('4')));
    let mut unnamed = sizing();
    for profile in &mut unnamed.profiles {
        profile.jobs[0].workflow = None;
        profile.jobs[0].name.clear();
    }
    app.update(Msg::Sizing(unnamed));
    let shown = screen(&app, 120, 24, false);
    assert!(
        rows_with(&shown, "halyard/storefront")
            .iter()
            .all(|row| row.contains("a job")),
        "as runnerctl sizing names it: {shown}"
    );
}
