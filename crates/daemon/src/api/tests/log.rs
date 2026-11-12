//! A job's log over the socket (ADR 0133): the plain log keeps its bytes and content type, the styled lines stream
//! as NDJSON a page at a time or followed, the outline and a search answer JSON, and a client reads a daemon without
//! these routes as not supporting them.

use std::time::Duration;

use reqwest::StatusCode;
use runnerd_core::api::{LogEvidence, LogOutline, LogSearch, LogSearchQuery, StyledLine};
use runnerd_core::client::Client;
use runnerd_core::github::{JobDetails, QueuedJob};
use runnerd_core::store::{LogPosition, RunnerRecord, Store};

use super::start;
use crate::testdir::Dir;

const POSITION: LogPosition = LogPosition { page: 1, offset: 0 };

/// Job 1 of `o/app`, which runner `runner-1` took, with `lines` of log.
async fn running(store: &Store, lines: &[&str]) {
    store.queued(
        "o/app",
        &QueuedJob {
            id: 1,
            run_id: 2,
            name: "build".to_owned(),
            ..QueuedJob::default()
        },
        1_000,
    );
    store.runner_started(RunnerRecord {
        purpose: None,
        admission: None,
        name: "runner-1".to_owned(),
        repository: "o/app".to_owned(),
        profile: "app".to_owned(),
        slot: 1,
        container: "c1".to_owned(),
        started_at: 1_000,
        memory_cap: Some(4 << 30),
        cpu_cap: Some(2000),
    });
    store.claimed(1, 2, "runner-1", 2_000);
    let lines = lines.iter().map(|line| (*line).to_owned()).collect();
    store.log(1, lines, POSITION, 1 << 20).await;
    store.flush().await;
}

fn finish(store: &Store) {
    store.details(JobDetails {
        id: 1,
        run_id: 2,
        run_attempt: 1,
        name: "build".to_owned(),
        workflow_name: None,
        head_branch: None,
        html_url: None,
        labels: Vec::new(),
        created_at: None,
        status: "completed".to_owned(),
        conclusion: Some("success".to_owned()),
        started_at: None,
        completed_at: None,
        runner_name: Some("runner-1".to_owned()),
        steps: Vec::new(),
    });
    store.runner_removed("runner-1", 3_000);
}

fn content_type(response: &reqwest::Response) -> String {
    response.headers()[reqwest::header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_owned()
}

fn styled(body: &str) -> Vec<StyledLine> {
    body.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn styled_lines_stream_as_ndjson_beside_the_plain_log_which_keeps_its_bytes() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    let store = &daemon.store;
    let coloured = "2026-10-23T01:00:00.5Z \u{1b}[31mred\u{1b}[0m";
    running(store, &[coloured, "##[group]Run make", "two"]).await;

    let plain = daemon
        .client
        .get("http://runnerd/v1/jobs/1/logs")
        .send()
        .await
        .unwrap();
    assert_eq!(content_type(&plain), "text/plain; charset=utf-8");
    assert_eq!(
        plain.text().await.unwrap(),
        format!("{coloured}\n##[group]Run make\ntwo\n"),
        "the plain log is the stored text, escapes and all"
    );

    let page = daemon
        .client
        .get("http://runnerd/v1/jobs/1/log/lines?from=1&limit=1")
        .send()
        .await
        .unwrap();
    assert_eq!(content_type(&page), "application/x-ndjson");
    let lines = styled(&page.text().await.unwrap());
    assert_eq!(lines.len(), 1, "a page holds its limit");
    assert_eq!((lines[0].line, lines[0].text()), (1, "Run make".to_owned()));

    let mut followed = daemon
        .client
        .get("http://runnerd/v1/jobs/1/log/lines?follow=true")
        .send()
        .await
        .unwrap();
    let mut body = String::new();
    while body.lines().count() < 3 {
        body.push_str(&String::from_utf8_lossy(
            &followed.chunk().await.unwrap().unwrap(),
        ));
    }
    store
        .log(1, vec!["\u{1b}[1mthree".to_owned()], POSITION, 1 << 20)
        .await;
    finish(store);
    let rest = tokio::time::timeout(Duration::from_secs(5), followed.text())
        .await
        .expect("the styled log did not end with its job")
        .unwrap();
    let lines = styled(&(body + &rest));
    assert_eq!(
        lines
            .iter()
            .map(|line| (line.line, line.text()))
            .collect::<Vec<_>>(),
        [
            (0, "red".to_owned()),
            (1, "Run make".to_owned()),
            (2, "two".to_owned()),
            (3, "three".to_owned()),
        ]
    );
    assert!(
        lines[3].runs[0].bold,
        "a line stored while followed is styled too"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn the_outline_and_a_search_answer_json_and_a_followed_search_is_refused() {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    running(
        &daemon.store,
        &[
            "##[group]Run make",
            "built",
            "##[endgroup]",
            "##[warning]slow",
        ],
    )
    .await;
    let client = Client::new(dir.0.join("runnerd.sock"));

    let outline: LogOutline = client.log_outline(1).await.unwrap();
    assert_eq!(outline.evidence, LogEvidence::Running);
    assert_eq!((outline.groups.len(), outline.warnings), (1, 1));
    let found: LogSearch = client
        .search_log(
            1,
            &LogSearchQuery {
                q: "BUILT".to_owned(),
                ..LogSearchQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (found.matches.as_slice(), found.next),
        ([1].as_slice(), None)
    );

    for query in ["q=x&follow=true", "q=", "follow=false"] {
        let response = daemon
            .client
            .get(format!("http://runnerd/v1/jobs/1/log/search?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    let missing = client.log_outline(9).await.unwrap_err();
    assert!(
        !missing.is_unsupported(),
        "a job the history lacks is not a route the daemon lacks: {missing}"
    );
    // What a daemon without these routes answers: the fallback every release has.
    let older = client
        .get::<LogOutline>("/v1/jobs/1/log/outline/later")
        .await
        .unwrap_err();
    assert!(older.is_unsupported(), "{older}");
    daemon.stop().await;
}
