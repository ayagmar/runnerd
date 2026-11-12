//! What needs attention, through the socket: the doctor's run as it starts, fails, overruns and finishes, requests
//! that wait for it, reloads that replace or remove what an item is about or change an insight, reloads in a row, a
//! rate limit that resets, and a restart (ADR 0132).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use runnerd_core::api::{
    Attention, AttentionKind, Check, CheckStatus, Doctor, DoctorState, ErrorResponse, Event,
    ListingFailure, ListingProblem, RepositoryStatus,
};
use runnerd_core::config::Config;

use super::{
    ATTENTION_TIMING, DOCTOR_TIMING, Daemon, EXAMPLE, doctor_cache, finished_jobs, memory_max,
    read_stream, start, start_doctored, sync_dir,
};
use crate::testdir::Dir;

/// A doctor that takes `delay` and warns of each repository the config serves, counting its runs in `runs`.
fn warns_of_each_repository(delay: Duration, runs: &Arc<AtomicUsize>) -> doctor_cache::Setup {
    let runs = runs.clone();
    doctor_cache::Setup {
        read: Arc::new(move |config: Option<&Config>, _: &Path, _| {
            runs.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(delay);
            config
                .map(Config::repository_names)
                .unwrap_or_default()
                .into_iter()
                .map(|name| Check {
                    name: "repository".to_owned(),
                    status: CheckStatus::Warn,
                    detail: format!("{name} is public"),
                    fix: None,
                })
                .collect()
        }),
        timing: DOCTOR_TIMING,
    }
}

async fn serving(doctor: doctor_cache::Setup) -> (Daemon, Dir) {
    let dir = Dir::new();
    let daemon = start_doctored(
        &dir.0,
        None,
        true,
        sync_dir,
        memory_max,
        EXAMPLE,
        doctor,
        ATTENTION_TIMING,
    )
    .await;
    (daemon, dir)
}

/// The next `count` `attention.changed` events on `events`: (instance, revision) of each.
async fn changes(events: &mut reqwest::Response, count: usize) -> Vec<(String, u64)> {
    read_stream(events, count, |envelope| {
        matches!(envelope.event, Event::AttentionChanged { .. })
    })
    .await
    .into_iter()
    .filter_map(|envelope| match envelope.event {
        Event::AttentionChanged { instance, revision } => Some((instance, revision)),
        _ => None,
    })
    .collect()
}

async fn events(daemon: &Daemon) -> reqwest::Response {
    daemon
        .client
        .get("http://runnerd/v1/events")
        .send()
        .await
        .unwrap()
}

async fn attention(daemon: &Daemon) -> Attention {
    let response = daemon
        .client
        .get("http://runnerd/v1/attention")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
}

/// The first snapshot `holds` is true of, within 5 s.
async fn until(daemon: &Daemon, holds: impl Fn(&Attention) -> bool) -> Attention {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let shown = attention(daemon).await;
        if holds(&shown) {
            return shown;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never came: {shown:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn kinds(attention: &Attention) -> Vec<AttentionKind> {
    attention.items.iter().map(|item| item.kind).collect()
}

fn titles(attention: &Attention) -> Vec<&str> {
    attention
        .items
        .iter()
        .map(|item| item.title.as_str())
        .collect()
}

/// The example with halyard/storefront's entry naming `name` instead.
fn serving_instead_of_storefront(name: &str) -> String {
    EXAMPLE.replace(
        "name = \"halyard/storefront\"",
        &format!("name = \"{name}\""),
    )
}

async fn replace(daemon: &Daemon, text: String) {
    let response = daemon
        .client
        .put("http://runnerd/v1/config")
        .body(text)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        response.text().await.unwrap()
    );
}

#[tokio::test]
async fn a_doctor_not_yet_run_is_said_and_its_finish_on_an_idle_host_reaches_clients() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (daemon, _dir) = serving(warns_of_each_repository(Duration::from_millis(500), &runs)).await;
    let mut events = events(&daemon).await;
    let before = attention(&daemon).await;
    assert_eq!(
        kinds(&before),
        [AttentionKind::DoctorUnavailable],
        "not an all clear: {before:#?}"
    );
    assert_eq!(titles(&before), ["Host checks have not finished yet"]);
    // Nothing else happens on the host: the doctor's finish alone is the news.
    let changed = changes(&mut events, 1).await;
    let after = attention(&daemon).await;
    assert_eq!(
        (after.instance.clone(), after.revision),
        changed[0],
        "the event names the snapshot to read"
    );
    assert_eq!(
        titles(&after),
        [
            "repository: halyard/ledger is public",
            "repository: halyard/storefront is public"
        ]
    );
    let response = daemon
        .client
        .get("http://runnerd/v1/doctor")
        .send()
        .await
        .unwrap();
    let doctor: Doctor = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert!(
        matches!(doctor.state, Some(DoctorState::Ready { .. })),
        "{doctor:?}"
    );
    assert_eq!(doctor.checks.len(), 2, "the same run, read from the cache");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "one run served both");
}

#[tokio::test]
async fn a_doctor_that_fails_or_runs_out_of_time_is_said_never_as_all_clear() {
    let failing = doctor_cache::Setup {
        read: Arc::new(|_: Option<&Config>, _: &Path, _| panic!("Docker hung up")),
        timing: DOCTOR_TIMING,
    };
    let slow = doctor_cache::Setup {
        timing: doctor_cache::Timing {
            deadline: Duration::from_secs(1),
            ..DOCTOR_TIMING
        },
        ..warns_of_each_repository(Duration::from_secs(3), &Arc::default())
    };
    for (doctor, why) in [(failing, "it stopped"), (slow, "it took longer than 1s")] {
        let (daemon, _dir) = serving(doctor).await;
        let shown = until(&daemon, |shown| {
            titles(shown)
                .first()
                .is_some_and(|title| title.starts_with("Host checks could not finish"))
        })
        .await;
        assert_eq!(kinds(&shown), [AttentionKind::DoctorUnavailable], "{why}");
        assert!(shown.items[0].title.contains(why), "{:?}", shown.items[0]);
        let response = daemon
            .client
            .get("http://runnerd/v1/doctor")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let refused: ErrorResponse =
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert!(refused.message.contains(why), "{refused:?}");
    }
}

#[tokio::test]
async fn a_reload_that_replaces_an_items_subject_is_a_new_revision_at_the_same_count() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (daemon, _dir) = serving(warns_of_each_repository(Duration::from_millis(300), &runs)).await;
    let first = until(&daemon, |shown| shown.items.len() == 2).await;
    let mut events = events(&daemon).await;
    replace(&daemon, serving_instead_of_storefront("halyard/docs")).await;
    // The first look after the reload: the old run is of a config no longer running, so its warnings go.
    let reloaded = until(&daemon, |shown| shown.revision != first.revision).await;
    assert!(
        !titles(&reloaded)
            .iter()
            .any(|title| title.contains("halyard/storefront")),
        "a removed repository's warning stays out: {reloaded:#?}"
    );
    let replaced = until(&daemon, |shown| {
        titles(shown)
            .iter()
            .any(|title| title.contains("halyard/docs"))
    })
    .await;
    assert_eq!(
        titles(&replaced),
        [
            "repository: halyard/docs is public",
            "repository: halyard/ledger is public"
        ],
        "by key, each check's own"
    );
    assert_eq!(replaced.items.len(), first.items.len(), "the same count");
    assert!(replaced.revision > first.revision);
    // Every new revision was announced, the last one too.
    let mut announced = Vec::new();
    while announced.last() != Some(&replaced.revision) {
        let (instance, revision) = changes(&mut events, 1).await.remove(0);
        assert_eq!(instance, replaced.instance, "one daemon");
        announced.push(revision);
    }
}

#[tokio::test]
async fn reloads_in_a_row_coalesce_into_one_run_for_the_last_config() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (daemon, _dir) = serving(warns_of_each_repository(Duration::from_millis(300), &runs)).await;
    until(&daemon, |shown| shown.items.len() == 2).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    for n in 1..=5 {
        replace(
            &daemon,
            serving_instead_of_storefront(&format!("halyard/docs-{n}")),
        )
        .await;
    }
    let last = until(&daemon, |shown| {
        titles(shown)
            .iter()
            .any(|title| title.contains("halyard/docs-5"))
    })
    .await;
    assert!(
        !titles(&last)
            .iter()
            .any(|title| title.contains("halyard/docs-") && !title.contains("docs-5")),
        "no run of a config replaced since is shown: {last:#?}"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let ran = runs.load(Ordering::SeqCst);
    assert!(
        (2..=3).contains(&ran),
        "the first, at most one the reloads overtook, and the last's: {ran} runs"
    );
}

#[tokio::test]
async fn a_restart_starts_a_new_revision_and_forgets_why_listings_failed() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    daemon.status.send_modify(|status| {
        status.repositories = vec![RepositoryStatus {
            name: "o/app".to_owned(),
            pools: Vec::new(),
            paused: false,
            queued_jobs: 0,
            listing: Some(ListingFailure {
                since: 1_800_000_000,
                problem: ListingProblem::Refused { status: 404 },
            }),
        }];
    });
    let before = until(&daemon, |shown| {
        kinds(shown).contains(&AttentionKind::GithubListing)
    })
    .await;
    daemon.stop().await;
    let restarted = start(&dir.0, None).await;
    let after = until(&restarted, |shown| {
        !kinds(shown).contains(&AttentionKind::DoctorUnavailable)
    })
    .await;
    assert_ne!(
        after.instance, before.instance,
        "a new instance: its revisions are not compared with the old one's"
    );
    assert_eq!(after.items, Vec::new(), "the failure is forgotten");
}

/// A doctor whose runs each take `delay`, counted in `runs`, at `timing`.
fn slow_doctor(
    delay: Duration,
    runs: &Arc<AtomicUsize>,
    timing: doctor_cache::Timing,
) -> doctor_cache::Setup {
    doctor_cache::Setup {
        timing,
        ..warns_of_each_repository(delay, runs)
    }
}

/// `GET /v1/doctor`: its status, its body, and how long it took.
async fn doctor(daemon: &Daemon) -> (StatusCode, Vec<u8>, Duration) {
    let asked = tokio::time::Instant::now();
    let response = daemon
        .client
        .get("http://runnerd/v1/doctor")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.bytes().await.unwrap().to_vec();
    (status, body, asked.elapsed())
}

#[tokio::test]
async fn a_run_past_its_deadline_is_the_only_one_until_it_ends_and_none_starts_after_a_stop() {
    let runs = Arc::new(AtomicUsize::new(0));
    let timing = doctor_cache::Timing {
        deadline: Duration::from_millis(500),
        answer: Duration::from_millis(200),
        ..DOCTOR_TIMING
    };
    let (daemon, _dir) = serving(slow_doctor(Duration::from_secs(4), &runs, timing)).await;
    until(&daemon, |shown| {
        titles(shown)
            .first()
            .is_some_and(|title| title.starts_with("Host checks could not finish"))
    })
    .await;
    // While the run that overran still reads the host, requests and a reload each want another.
    for _ in 0..3 {
        let (status, body, took) = doctor(&daemon).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let refused: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert!(refused.message.contains("took longer"), "{refused:?}");
        assert!(took < Duration::from_secs(1), "answered at once: {took:?}");
    }
    replace(&daemon, serving_instead_of_storefront("halyard/docs")).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "no second run reads the host beside the one that overran"
    );
    daemon.stop().await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the run the reload wanted never starts once the daemon stopped"
    );
}

#[tokio::test]
async fn a_request_waits_for_a_run_at_most_its_bound_and_then_answers_the_last_run_as_it_stands() {
    let runs = Arc::new(AtomicUsize::new(0));
    // Each run takes 1.5 s; a request waits 200 ms; a run is stale 2 s after it ended.
    let timing = doctor_cache::Timing {
        fresh: Duration::from_secs(1),
        stale: Duration::from_secs(1),
        deadline: Duration::from_secs(10),
        answer: Duration::from_millis(200),
        ..DOCTOR_TIMING
    };
    let (daemon, _dir) = serving(slow_doctor(Duration::from_millis(1500), &runs, timing)).await;
    // At the start, while the first run goes on: unavailable, at once, however many ask.
    let asked = futures_util::future::join_all((0..4).map(|_| doctor(&daemon))).await;
    for (status, body, took) in asked {
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let refused: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(refused.message, "Host checks have not finished yet");
        assert!(took < Duration::from_secs(1), "answered at once: {took:?}");
    }
    let ready = loop {
        let (status, body, _) = doctor(&daemon).await;
        if status == StatusCode::OK {
            break serde_json::from_slice::<Doctor>(&body).unwrap();
        }
    };
    assert!(
        matches!(ready.state, Some(DoctorState::Ready { .. })),
        "{ready:?}"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1, "nothing asked for another");
    // Stale now: requests at once ask for one run, and each answers the stale checks, marked so, before it ends.
    let asked = futures_util::future::join_all((0..4).map(|_| doctor(&daemon))).await;
    for (status, body, took) in asked {
        assert_eq!(status, StatusCode::OK);
        let stale: Doctor = serde_json::from_slice(&body).unwrap();
        assert!(
            matches!(stale.state, Some(DoctorState::Stale { .. })),
            "{stale:?}"
        );
        assert_eq!(stale.checks, ready.checks, "the last run's checks");
        assert!(took < Duration::from_secs(1), "answered at once: {took:?}");
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the requests shared one run, and none ran again after it"
    );
}

#[tokio::test]
async fn a_reload_that_starts_or_ends_an_insight_shows_before_the_insights_would_be_read_again() {
    let dir = Dir::new();
    let daemon = start_doctored(
        &dir.0,
        None,
        true,
        sync_dir,
        memory_max,
        EXAMPLE,
        super::passing_doctor(),
        crate::api::attention::Timing {
            insights: Duration::from_hours(1),
            ..ATTENTION_TIMING
        },
    )
    .await;
    // node's jobs peak at 6.5G of its 7G, and auto sizing is off: a warning.
    finished_jobs(&daemon.store, "node", 7 << 30, 6656 << 20).await;
    daemon.store.flush().await;
    let near_cap = |shown: &Attention| titles(shown).iter().any(|title| title.contains("peaks at"));
    replace(&daemon, format!("{EXAMPLE}\n# Read again.\n")).await;
    let warned = until(&daemon, near_cap).await;
    assert!(
        kinds(&warned).contains(&AttentionKind::Insight),
        "{warned:#?}"
    );
    // Auto sizing on: it raises node itself, so the insight is a notice, not listed.
    replace(&daemon, format!("{EXAMPLE}\n[sizing]\nauto = true\n")).await;
    until(&daemon, |shown| !near_cap(shown)).await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_rate_limit_that_resets_leaves_the_list_with_no_new_status() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    daemon
        .status
        .send_modify(|status| status.github_limited_until = Some(now + 2));
    let limited = until(&daemon, |shown| {
        kinds(shown).contains(&AttentionKind::RateLimited)
    })
    .await;
    let mut events = events(&daemon).await;
    // The engine publishes nothing more: the reset time alone ends it.
    let reset = until(&daemon, |shown| {
        !kinds(shown).contains(&AttentionKind::RateLimited)
    })
    .await;
    assert_eq!(reset.instance, limited.instance);
    assert!(reset.revision > limited.revision);
    let mut announced = changes(&mut events, 1).await;
    while announced.last().map(|(_, revision)| *revision) != Some(reset.revision) {
        announced.extend(changes(&mut events, 1).await);
    }
    daemon.stop().await;
}
