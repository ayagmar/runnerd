//! The job history's search, sorts, timeline and compare over the socket (ADR 0135): real query URLs, the cursor
//! carried from one page to the next, and what the daemon refuses, worded by core.

use reqwest::StatusCode;
use runnerd_core::api::{Baseline, JobComparison, JobList, SlotTimeline, Unavailable};
use runnerd_core::github::QueuedJob;
use runnerd_core::store::Store;

use super::{Daemon, start};
use crate::testdir::Dir;

/// 2026-10-23T01:00:00Z, in seconds.
const T0: u64 = 1_792_717_200;

/// Jobs 1 to 5 of `o/app`, queued a second apart; 2 and 4 named `b_uild`, the rest `build`.
fn queued(store: &Store) {
    for id in 1..=5 {
        store.queued(
            "o/app",
            &QueuedJob {
                id,
                run_id: id,
                name: if id % 2 == 0 { "b_uild" } else { "build" }.to_owned(),
                workflow: Some("CI".to_owned()),
                branch: Some("main".to_owned()),
                created_at: (T0 + id) * 1000,
                ..QueuedJob::default()
            },
            (T0 + id) * 1000,
        );
    }
}

async fn get(daemon: &Daemon, path: &str) -> reqwest::Response {
    daemon
        .client
        .get(format!("http://runnerd{path}"))
        .send()
        .await
        .unwrap()
}

async fn message(response: reqwest::Response) -> String {
    let body: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    body["message"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn a_search_pages_with_its_cursor_and_a_query_the_daemon_refuses_answers_400_in_cores_words()
{
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    queued(&daemon.store);
    daemon.store.flush().await;

    // `_` is text: `b_u` finds the two named `b_uild`, not `build`.
    let mut path = "/v1/jobs?q=b_u&sort=queued&order=asc&limit=1".to_owned();
    let mut seen = Vec::new();
    loop {
        let response = get(&daemon, &path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let list: JobList = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert!(list.understood(), "this daemon says what it read");
        seen.extend(list.page.jobs.iter().map(|job| job.id));
        let Some(cursor) = list.cursor else { break };
        path = format!(
            "/v1/jobs?q=b_u&sort=queued&order=asc&limit=1&cursor={}",
            urlencode(&cursor)
        );
    }
    assert_eq!(seen, [2, 4]);

    let first: JobList = get(&daemon, "/v1/jobs?sort=wait&limit=1")
        .await
        .text()
        .await
        .map(|text| serde_json::from_str(&text).unwrap())
        .unwrap();
    let cursor = urlencode(&first.cursor.unwrap());
    let refused = [
        ("/v1/jobs?q=".to_owned(), "a search is 1 to 100 bytes"),
        (
            "/v1/jobs?sort=wait&before=3".to_owned(),
            "before pages the list by ID",
        ),
        (
            format!("/v1/jobs?sort=wait&branch=main&cursor={cursor}"),
            "the cursor is of another sort or search",
        ),
        (
            format!("/v1/jobs?cursor={cursor}"),
            "a cursor pages a sorted list",
        ),
        (
            "/v1/jobs?sort=wait&cursor=j9.x".to_owned(),
            "the cursor is not one this daemon made",
        ),
        ("/v1/jobs?sort=disk".to_owned(), ""),
    ];
    for (path, words) in refused {
        let response = get(&daemon, &path).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        if !words.is_empty() {
            let said = message(response).await;
            assert!(said.starts_with(words), "{path}: {said}");
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn the_timeline_and_compare_answer_json_and_say_what_they_cannot() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    queued(&daemon.store);
    daemon.store.flush().await;

    let timeline: SlotTimeline = get(
        &daemon,
        &format!("/v1/timeline?since={T0}&until={}", T0 + 60),
    )
    .await
    .text()
    .await
    .map(|text| serde_json::from_str(&text).unwrap())
    .unwrap();
    assert_eq!(
        (timeline.since, timeline.until, timeline.unassigned.len()),
        (T0, T0 + 60, 5),
        "the queued jobs wait apart from any slot"
    );
    for path in [
        format!("/v1/timeline?since={T0}&until={T0}"),
        format!("/v1/timeline?since={}&until={T0}", T0 - 8 * 24 * 3600),
    ] {
        assert_eq!(
            get(&daemon, &path).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }

    let compared: JobComparison = get(&daemon, "/v1/jobs/5/compare")
        .await
        .text()
        .await
        .map(|text| serde_json::from_str(&text).unwrap())
        .unwrap();
    assert_eq!(
        compared.baseline,
        Baseline::Unavailable {
            reason: Unavailable::NoEarlierSuccess
        },
        "queued jobs are no successful run"
    );
    let other: JobComparison = get(&daemon, "/v1/jobs/5/compare?with=4")
        .await
        .text()
        .await
        .map(|text| serde_json::from_str(&text).unwrap())
        .unwrap();
    assert_eq!(
        other.baseline,
        Baseline::Incompatible { job: 4 },
        "another name"
    );
    for path in ["/v1/jobs/9/compare", "/v1/jobs/5/compare?with=9"] {
        assert_eq!(
            get(&daemon, path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    daemon.stop().await;
}

/// `text` for a query string: a cursor holds only letters, digits, `.` and `-`, which need no escape, so this
/// escapes nothing a cursor holds; it is here so a cursor that did would fail loudly.
fn urlencode(text: &str) -> String {
    assert!(
        text.chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-')),
        "{text}"
    );
    text.to_owned()
}
