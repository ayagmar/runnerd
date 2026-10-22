use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use super::{Deleted, Error, Github, GithubApi, QueuedJob, Token};

const TOKEN: &str = "ghp_sentinelTokenValue0123456789";

/// A request the fake GitHub received.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    uri: String,
    headers: HeaderMap,
}

type Handler = dyn Fn(&Seen) -> Response + Send + Sync;

/// Serves `handler` on a loopback port in place of api.github.com; returns its URL and what it received.
async fn fake_github(
    handler: impl Fn(&Seen) -> Response + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler: Arc<Handler> = Arc::new(handler);
    let log = seen.clone();
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
        let request = Seen {
            method,
            uri: uri.to_string(),
            headers,
        };
        log.lock().unwrap().push(request.clone());
        let response = handler(&request);
        async move { response }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

fn client(url: &str) -> Github {
    Github::new(url, Box::new(|| Ok(Token(TOKEN.to_owned())))).unwrap()
}

fn json(body: &str) -> Response {
    ([("content-type", "application/json")], body.to_owned()).into_response()
}

#[tokio::test]
async fn queued_jobs_come_from_queued_and_started_runs() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => {
            json(r#"{"total_count":1,"workflow_runs":[{"id":10}]}"#)
        }
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            json(r#"{"total_count":1,"workflow_runs":[{"id":20}]}"#)
        }
        "/repos/o/r/actions/runs/10/jobs?filter=latest&per_page=100" => json(
            r#"{"jobs":[{"id":1,"run_id":10,"name":"smoke","status":"queued","labels":["self-hosted","sandbox"]}]}"#,
        ),
        // A matrix run: one job started, one still waiting for a runner.
        "/repos/o/r/actions/runs/20/jobs?filter=latest&per_page=100" => json(
            r#"{"jobs":[{"id":2,"run_id":20,"name":"a","status":"in_progress","labels":["self-hosted"]},
                       {"id":3,"run_id":20,"name":"b","status":"queued","labels":["self-hosted"]}]}"#,
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let jobs = client(&url).queued_jobs("o/r").await.unwrap();
    assert_eq!(
        jobs,
        [
            QueuedJob {
                id: 1,
                run_id: 10,
                name: "smoke".to_owned(),
                labels: vec!["self-hosted".to_owned(), "sandbox".to_owned()],
            },
            QueuedJob {
                id: 3,
                run_id: 20,
                name: "b".to_owned(),
                labels: vec!["self-hosted".to_owned()],
            },
        ]
    );
}

#[tokio::test]
async fn an_unchanged_answer_is_served_from_the_last_response() {
    let (url, seen) = fake_github(|request| {
        if request.headers.get("if-none-match").map(|v| v.as_bytes()) == Some(b"\"v1\"") {
            return StatusCode::NOT_MODIFIED.into_response();
        }
        (
            [("etag", "\"v1\"")],
            json(r#"{"runners":[{"id":7,"name":"runnerd-a","status":"online","busy":true}]}"#),
        )
            .into_response()
    })
    .await;
    let github = client(&url);
    let first = github.runners("o/r").await.unwrap();
    let second = github.runners("o/r").await.unwrap();
    assert_eq!(first, second);
    assert_eq!(second[0].id, 7);
    assert!(
        seen.lock().unwrap()[1]
            .headers
            .contains_key("if-none-match")
    );
}

#[tokio::test]
async fn a_used_up_rate_limit_delays_requests_until_its_reset() {
    let reset = SystemTime::now() + Duration::from_secs(3600);
    let reset_secs = reset
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (url, seen) = fake_github(move |_| {
        (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "0".to_owned()),
                ("x-ratelimit-reset", reset_secs.to_string()),
            ],
            r#"{"message":"API rate limit exceeded"}"#,
        )
            .into_response()
    })
    .await;
    let github = client(&url);
    let expected = SystemTime::UNIX_EPOCH + Duration::from_secs(reset_secs);
    assert!(matches!(
        github.runners("o/r").await,
        Err(Error::RateLimited { until }) if until == expected
    ));
    assert_eq!(github.blocked_until(), Some(expected));
    // The next poll is not sent at all before the reset.
    assert!(matches!(
        github.queued_jobs("o/r").await,
        Err(Error::RateLimited { .. })
    ));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_secondary_rate_limit_waits_for_retry_after() {
    let (url, _) =
        fake_github(|_| (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")]).into_response())
            .await;
    let github = client(&url);
    let before = SystemTime::now();
    assert!(matches!(
        github.runners("o/r").await,
        Err(Error::RateLimited { .. })
    ));
    let until = github.blocked_until().unwrap();
    assert!(until >= before + Duration::from_secs(60));
}

#[tokio::test]
async fn a_rejected_token_is_read_again_once() {
    let (url, seen) = fake_github(|request| {
        if request.headers.get("authorization").map(|v| v.as_bytes()) == Some(b"Bearer new") {
            json(r#"{"runners":[]}"#)
        } else {
            StatusCode::UNAUTHORIZED.into_response()
        }
    })
    .await;
    let reads = Arc::new(Mutex::new(0));
    let counter = reads.clone();
    let github = Github::new(
        &url,
        Box::new(move || {
            let mut reads = counter.lock().unwrap();
            *reads += 1;
            Ok(Token(if *reads == 1 { "old" } else { "new" }.to_owned()))
        }),
    )
    .unwrap();
    assert_eq!(github.runners("o/r").await.unwrap(), []);
    assert_eq!(*reads.lock().unwrap(), 2);
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn deleting_a_runner_tells_gone_and_busy_apart() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runners/1" => StatusCode::NO_CONTENT.into_response(),
        "/repos/o/r/actions/runners/2" => StatusCode::NOT_FOUND.into_response(),
        "/repos/o/r/actions/runners/3" => (
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"message":"Runner is still running a job"}"#,
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    })
    .await;
    let github = client(&url);
    assert_eq!(
        github.delete_runner("o/r", 1).await.unwrap(),
        Deleted::Deleted
    );
    assert_eq!(
        github.delete_runner("o/r", 2).await.unwrap(),
        Deleted::NotFound
    );
    assert_eq!(github.delete_runner("o/r", 3).await.unwrap(), Deleted::Busy);
    assert!(github.delete_runner("o/r", 4).await.is_err());
}

#[tokio::test]
async fn a_jit_config_registers_the_runner_with_the_repositorys_labels() {
    let (url, seen) = fake_github(|request| {
        assert_eq!(request.method, Method::POST);
        (
            StatusCode::CREATED,
            json(r#"{"runner":{"id":42,"name":"runnerd-h-p-1","status":"offline","busy":false},"encoded_jit_config":"Y29uZmln"}"#),
        )
            .into_response()
    })
    .await;
    let labels = ["self-hosted".to_owned(), "sandbox".to_owned()];
    let (runner, config) = client(&url)
        .jit_config("o/r", "runnerd-h-p-1", &labels)
        .await
        .unwrap();
    assert_eq!(runner.id, 42);
    assert_eq!(config.0, "Y29uZmln");
    assert_eq!(
        seen.lock().unwrap()[0].uri,
        "/repos/o/r/actions/runners/generate-jitconfig"
    );
}

#[tokio::test]
async fn the_token_never_shows_in_debug_output_or_errors() {
    let (url, seen) = fake_github(|request| {
        if request.uri.contains("runners/9") {
            (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response()
        } else {
            StatusCode::UNAUTHORIZED.into_response()
        }
    })
    .await;
    let github = client(&url);
    let unauthorized = github.runners("o/r").await.unwrap_err();
    let server_error = github.delete_runner("o/r", 9).await.unwrap_err();
    // The token did go out, so its absence below is not for lack of use.
    let sent = seen.lock().unwrap()[0].headers["authorization"].clone();
    assert_eq!(sent, format!("Bearer {TOKEN}").as_str());
    let unreachable = client("http://127.0.0.1:1")
        .runners("o/r")
        .await
        .unwrap_err();
    for text in [
        format!("{github:?}"),
        format!("{:?}", Token(TOKEN.to_owned())),
        format!("{unauthorized:?} {unauthorized}"),
        format!("{server_error:?} {server_error}"),
        format!("{unreachable:?} {unreachable}"),
    ] {
        assert!(!text.contains(TOKEN), "{text}");
    }
}

#[tokio::test]
async fn a_run_that_left_the_queue_is_no_longer_cached() {
    let polls = Arc::new(Mutex::new(0));
    let counter = polls.clone();
    let (url, seen) = fake_github(move |request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => {
            let mut polls = counter.lock().unwrap();
            *polls += 1;
            // Listed on the first and third poll, gone on the second.
            if *polls == 2 {
                json(r#"{"workflow_runs":[]}"#)
            } else {
                json(r#"{"workflow_runs":[{"id":10}]}"#)
            }
        }
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            json(r#"{"workflow_runs":[]}"#)
        }
        _ => (
            [("etag", "\"jobs\"")],
            json(r#"{"jobs":[{"id":1,"run_id":10,"name":"a","status":"queued","labels":[]}]}"#),
        )
            .into_response(),
    })
    .await;
    let github = client(&url);
    for _ in 0..3 {
        github.queued_jobs("o/r").await.unwrap();
    }
    let jobs_requests: Vec<Seen> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.uri.contains("/jobs?"))
        .cloned()
        .collect();
    assert_eq!(jobs_requests.len(), 2);
    assert!(!jobs_requests[1].headers.contains_key("if-none-match"));
}
