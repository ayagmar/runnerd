//! The GitHub client against a fake GitHub on a loopback port: queued jobs, listings over several pages,
//! conditional requests and how much of them is kept, rate limits, token refresh, runner registration and deletion, who started a run and opened a pull request, and that the token
//! never shows in `Debug` output or errors.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;

use super::{
    ActionFailure, Class, Conditional, Deleted, Error, Github, GithubApi, JobActionRequest,
    QueuedJob, RunNow, Token,
};
use crate::sync::lock;

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
                run_attempt: 1,
                ..QueuedJob::default()
            },
            QueuedJob {
                id: 3,
                run_id: 20,
                name: "b".to_owned(),
                labels: vec!["self-hosted".to_owned()],
                run_attempt: 1,
                ..QueuedJob::default()
            },
        ]
    );
}

#[tokio::test]
async fn an_unchanged_answer_is_served_from_the_last_response() {
    let (url, seen) = fake_github(|request| {
        if request
            .headers
            .get("if-none-match")
            .map(HeaderValue::as_bytes)
            == Some(b"\"v1\"")
        {
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

/// `body`, with a `Link` to a next page if `next`, tagged by its URI: a request conditional on that tag gets a 304.
fn page(request: &Seen, body: &str, next: bool) -> Response {
    let etag = format!("\"{}\"", request.uri);
    if request
        .headers
        .get("if-none-match")
        .is_some_and(|sent| sent.as_bytes() == etag.as_bytes())
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let mut response = ([("etag", etag)], json(body)).into_response();
    if next {
        // GitHub links pages by repository ID; the client builds its own next address.
        let link = r#"<https://api.github.com/repositories/1/elsewhere?page=2>; rel="next", <https://api.github.com/repositories/1/elsewhere?page=9>; rel="last""#;
        response
            .headers_mut()
            .insert("link", HeaderValue::from_static(link));
    }
    response
}

fn uris(seen: &Mutex<Vec<Seen>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|request| request.uri.clone())
        .collect()
}

#[tokio::test]
async fn a_queued_job_on_a_later_page_of_runs_and_jobs_is_found() {
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => {
            page(request, r#"{"workflow_runs":[{"id":10}]}"#, true)
        }
        // Run 10 moved down a page while the first was read.
        "/repos/o/r/actions/runs?status=queued&per_page=100&page=2" => {
            page(request, r#"{"workflow_runs":[{"id":10},{"id":20}]}"#, false)
        }
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            page(request, r#"{"workflow_runs":[]}"#, false)
        }
        "/repos/o/r/actions/runs/10/jobs?filter=latest&per_page=100" => page(
            request,
            r#"{"jobs":[{"id":1,"run_id":10,"name":"a","status":"completed","labels":[]}]}"#,
            false,
        ),
        // A matrix whose first page is done; the only job still waiting is on the second.
        "/repos/o/r/actions/runs/20/jobs?filter=latest&per_page=100" => page(
            request,
            r#"{"jobs":[{"id":2,"run_id":20,"name":"m (1)","status":"completed","labels":[]}]}"#,
            true,
        ),
        "/repos/o/r/actions/runs/20/jobs?filter=latest&per_page=100&page=2" => page(
            request,
            r#"{"jobs":[{"id":3,"run_id":20,"name":"m (101)","status":"queued","labels":["self-hosted"]}]}"#,
            false,
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let github = client(&url);
    let first = github.queued_jobs("o/r").await.unwrap();
    let sent = seen.lock().unwrap().len();
    let second = github.queued_jobs("o/r").await.unwrap();
    for jobs in [&first, &second] {
        assert_eq!(
            jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
            [3],
            "the queued job on page 2 of run 20, found again from 304s"
        );
    }
    assert_eq!(
        uris(&seen)
            .iter()
            .filter(|uri| uri.contains("/runs/10/jobs"))
            .count(),
        2,
        "run 10 is read once a poll though it showed on two pages"
    );
    assert!(
        seen.lock().unwrap()[sent..]
            .iter()
            .all(|request| request.headers.contains_key("if-none-match")),
        "every page of the second poll is conditional"
    );
}

#[tokio::test]
async fn an_orphan_runner_on_a_later_page_is_listed() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runners?per_page=100" => page(
            request,
            r#"{"runners":[{"id":1,"name":"runnerd-h-p-1","status":"online","busy":true}]}"#,
            true,
        ),
        "/repos/o/r/actions/runners?per_page=100&page=2" => page(
            request,
            r#"{"runners":[{"id":1,"name":"runnerd-h-p-1","status":"online","busy":true},
                           {"id":2,"name":"runnerd-h-p-2","status":"offline","busy":false}]}"#,
            false,
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let runners = client(&url).runners("o/r").await.unwrap();
    assert_eq!(
        runners.iter().map(|runner| runner.id).collect::<Vec<_>>(),
        [1, 2]
    );
}

#[tokio::test]
async fn a_listing_that_never_ends_is_read_ten_pages_deep() {
    let (url, seen) = fake_github(|request| page(request, r#"{"runners":[]}"#, true)).await;
    client(&url).runners("o/r").await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 10);
}

/// A listing of runs `ids` split into pages of 100, answering for page `number`.
fn runs_page(request: &Seen, ids: &[u64], number: usize) -> Response {
    let pages: Vec<&[u64]> = ids.chunks(100).collect();
    let runs: Vec<String> = pages
        .get(number - 1)
        .map_or(&[][..], |page| *page)
        .iter()
        .map(|id| format!(r#"{{"id":{id}}}"#))
        .collect();
    page(
        request,
        &format!(r#"{{"workflow_runs":[{}]}}"#, runs.join(",")),
        number < pages.len(),
    )
}

#[tokio::test]
async fn a_busy_repository_is_polled_queued_runs_first_then_the_oldest_within_the_old_run_count() {
    // 250 runs in progress on GitHub's own runners, and 150 newer queued runs, each listed newest first.
    let (url, seen) = fake_github(|request| {
        let uri = request.uri.as_str();
        let number = |rest: &str| {
            rest.strip_prefix("&page=")
                .map_or(1, |number| number.parse().unwrap())
        };
        if let Some(rest) = uri.strip_prefix("/repos/o/r/actions/runs?status=queued&per_page=100") {
            return runs_page(request, &(301..=450).rev().collect::<Vec<_>>(), number(rest));
        }
        if let Some(rest) =
            uri.strip_prefix("/repos/o/r/actions/runs?status=in_progress&per_page=100")
        {
            return runs_page(request, &(1..=250).rev().collect::<Vec<_>>(), number(rest));
        }
        let (run, rest) = uri
            .trim_start_matches("/repos/o/r/actions/runs/")
            .split_once("/jobs?")
            .unwrap();
        let run: u64 = run.parse().unwrap();
        let job = |id: u64, status: &str| {
            format!(r#"{{"jobs":[{{"id":{id},"run_id":{run},"name":"j","status":"{status}","labels":[]}}]}}"#)
        };
        // Run 50, the last read, has its queued job on a second page.
        match (run, rest.ends_with("&page=2")) {
            (50, false) => page(request, &job(5000, "completed"), true),
            _ => page(request, &job(run, "queued"), false),
        }
    })
    .await;
    let jobs = client(&url).queued_jobs("o/r").await.unwrap();
    assert_eq!(
        jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
        (1..=50).chain(301..=450).collect::<Vec<_>>(),
        "every queued run, then the oldest runs in progress up to 200 runs, the last read to its end"
    );
    // Five pages of runs, 200 runs' jobs, and run 50's second page.
    assert_eq!(seen.lock().unwrap().len(), 206);
}

/// Serves job `id` as completed when `id` is in `completed`, otherwise in progress with a name `pad` long.
async fn fake_jobs(
    completed: std::ops::RangeInclusive<u64>,
    pad: usize,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_github(move |request| {
        let id: u64 = request
            .uri
            .trim_start_matches("/repos/o/r/actions/jobs/")
            .parse()
            .unwrap();
        let (status, conclusion) = if completed.contains(&id) {
            ("completed", r#""success""#)
        } else {
            ("in_progress", "null")
        };
        let name = "n".repeat(pad);
        page(
            request,
            &format!(
                r#"{{"id":{id},"run_id":1,"name":"{name}","status":"{status}","conclusion":{conclusion}}}"#
            ),
            false,
        )
    })
    .await
}

fn conditional(seen: &Mutex<Vec<Seen>>, uri: &str) -> Vec<bool> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|request| request.uri == uri)
        .map(|request| request.headers.contains_key("if-none-match"))
        .collect()
}

#[tokio::test]
async fn completed_jobs_leave_the_cache_while_an_active_job_stays_conditional() {
    let (url, seen) = fake_jobs(2..=200, 0).await;
    let github = client(&url);
    github.job("o/r", 1).await.unwrap();
    for id in 2..=200 {
        assert_eq!(github.job("o/r", id).await.unwrap().status, "completed");
    }
    let active = github.job("o/r", 1).await.unwrap();
    assert_eq!(
        active.status, "in_progress",
        "read from the 304's kept answer"
    );
    assert_eq!(
        conditional(&seen, "/repos/o/r/actions/jobs/1"),
        [false, true]
    );
    assert_eq!(
        lock(&github.etags).entries.len(),
        1,
        "only the active job's answer is kept"
    );
}

#[tokio::test]
async fn kept_answers_stay_within_their_byte_limit_and_the_least_recently_used_go() {
    let (url, seen) = fake_jobs(0..=0, 400).await;
    let mut github = client(&url);
    let limit = 2_000;
    // Every answer may go at once: least recently used first.
    *github.etags.get_mut().unwrap() = Conditional::new(limit, Duration::ZERO);
    for id in 2..=20 {
        github.job("o/r", id).await.unwrap();
        // Job 1 is asked between every other: the most recently used.
        github.job("o/r", 1).await.unwrap();
        assert!(
            lock(&github.etags).bytes <= limit,
            "within {limit} bytes after job {id}"
        );
    }
    github.job("o/r", 2).await.unwrap();
    assert!(
        conditional(&seen, "/repos/o/r/actions/jobs/1")[1..]
            .iter()
            .all(|sent| *sent),
        "job 1 stays conditional"
    );
    assert_eq!(
        conditional(&seen, "/repos/o/r/actions/jobs/2"),
        [false, false],
        "job 2, unused the longest, was dropped"
    );
}

#[tokio::test]
async fn polls_that_ask_more_than_the_cache_holds_keep_the_answers_that_fit_conditional() {
    let (url, seen) = fake_jobs(0..=0, 400).await;
    let mut github = client(&url);
    let limit = 2_000;
    *github.etags.get_mut().unwrap() = Conditional::new(limit, Duration::from_secs(600));
    // Two polls asking the same ten jobs in the same order, five times what the cache holds.
    for _ in 0..2 {
        for id in 1..=10 {
            github.job("o/r", id).await.unwrap();
        }
    }
    assert!(lock(&github.etags).bytes <= limit, "within {limit} bytes");
    for id in 1..=2 {
        assert_eq!(
            conditional(&seen, &format!("/repos/o/r/actions/jobs/{id}")),
            [false, true],
            "job {id}, kept from the first poll, is conditional in the second"
        );
    }
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
        if request
            .headers
            .get("authorization")
            .map(HeaderValue::as_bytes)
            == Some(b"Bearer new")
        {
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

/// A client whose token source counts its reads and gives `old` first, `new` after.
fn rotating_client(url: &str, reads: &Arc<Mutex<u32>>, wait: Duration) -> Github {
    let counter = reads.clone();
    Github::new(
        url,
        Box::new(move || {
            std::thread::sleep(wait);
            let mut reads = counter.lock().unwrap();
            *reads += 1;
            Ok(Token(if *reads == 1 { "old" } else { "new" }.to_owned()))
        }),
    )
    .unwrap()
}

async fn new_token_only() -> String {
    let (url, _) = fake_github(|request| {
        if request
            .headers
            .get("authorization")
            .map(HeaderValue::as_bytes)
            == Some(b"Bearer new")
        {
            json(r#"{"runners":[]}"#)
        } else {
            StatusCode::UNAUTHORIZED.into_response()
        }
    })
    .await;
    url
}

#[tokio::test]
async fn requests_refused_together_read_the_token_again_once() {
    let url = new_token_only().await;
    let reads = Arc::new(Mutex::new(0));
    let github = rotating_client(&url, &reads, Duration::ZERO);
    let (a, b, c, d) = tokio::join!(
        github.runners("o/a"),
        github.runners("o/b"),
        github.runners("o/c"),
        github.runners("o/d")
    );
    for answer in [a, b, c, d] {
        assert_eq!(answer.unwrap(), []);
    }
    assert_eq!(*reads.lock().unwrap(), 2);
}

#[tokio::test]
async fn a_slow_token_source_does_not_hold_up_other_tasks() {
    let url = new_token_only().await;
    let reads = Arc::new(Mutex::new(0));
    let github = rotating_client(&url, &reads, Duration::from_millis(300));
    let ticks = Arc::new(Mutex::new(0));
    let ticker = tokio::spawn({
        let ticks = ticks.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                *ticks.lock().unwrap() += 1;
            }
        }
    });
    assert_eq!(github.runners("o/r").await.unwrap(), []);
    ticker.abort();
    // Two reads of 300 ms each: a source run on this thread would leave the ticker none.
    assert!(*ticks.lock().unwrap() > 20, "other tasks ran meanwhile");
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

/// Lets every job action go.
fn go() -> BoxFuture<'static, bool> {
    Box::pin(async { true })
}

/// A body that starts and never ends.
fn endless() -> axum::body::Body {
    axum::body::Body::from_stream(
        futures_util::stream::once(async { Ok::<_, std::io::Error>("{\"id\":") })
            .chain(futures_util::stream::pending()),
    )
}

/// GitHub acknowledges a job action with 201 or 202 and a body that may be empty, `{}`, or slow to come: the status
/// is the answer, the body is never read, and each is sent once. A refusal is its status alone, whatever its body
/// says (ADR 0134).
#[tokio::test]
async fn a_job_action_is_its_status_its_body_unread_and_it_is_sent_once() {
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs/10/cancel" => (StatusCode::ACCEPTED, endless()).into_response(),
        "/repos/o/r/actions/jobs/11/rerun" => (StatusCode::CREATED, json("{}")).into_response(),
        "/repos/o/r/actions/runs/10/rerun-failed-jobs" => {
            (StatusCode::CREATED, "not json at all").into_response()
        }
        _ => (
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible with ghp_FAKE_SENTINEL_0123"}"#,
        )
            .into_response(),
    })
    .await;
    let github = client(&url);
    let github = &github;
    let sent = |request| async move {
        tokio::time::timeout(
            Duration::from_secs(5),
            github.job_action("o/r", request, &go),
        )
        .await
        .expect("answered without waiting for the body")
    };
    assert_eq!(
        sent(JobActionRequest::CancelRun { run: 10 }).await,
        Ok(202),
        "a 202 whose body never ends is accepted"
    );
    assert_eq!(sent(JobActionRequest::RerunJob { job: 11 }).await, Ok(201));
    assert_eq!(
        sent(JobActionRequest::RerunFailed { run: 10 }).await,
        Ok(201)
    );
    let refused = sent(JobActionRequest::RerunJob { job: 12 }).await;
    assert_eq!(refused, Err(ActionFailure::Status { status: 403 }));
    assert!(
        !format!("{refused:?}").contains("FAKE_SENTINEL"),
        "GitHub's text stays in Github"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "each sent once: {seen:?}");
    assert!(seen.iter().all(|seen| seen.method == Method::POST));
}

/// A run's attempt and whether it completed, for a job action; a failure as its status alone.
#[tokio::test]
async fn a_job_actions_read_of_the_run_is_its_attempt_and_whether_it_completed() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs/10" => {
            json(r#"{"id":10,"run_attempt":3,"status":"completed","event":"push","head_sha":"a"}"#)
        }
        "/repos/o/r/actions/runs/11" => {
            json(r#"{"id":11,"status":"in_progress","event":"push","head_sha":"a"}"#)
        }
        _ => (
            StatusCode::NOT_FOUND,
            r#"{"message":"Not Found ghp_FAKE_SENTINEL"}"#,
        )
            .into_response(),
    })
    .await;
    let github = client(&url);
    assert_eq!(
        github.action_run("o/r", 10).await,
        Ok(RunNow {
            attempt: 3,
            completed: true
        })
    );
    assert_eq!(
        github.action_run("o/r", 11).await,
        Ok(RunNow {
            attempt: 1,
            completed: false
        })
    );
    assert_eq!(
        github.action_run("o/r", 12).await,
        Err(ActionFailure::Status { status: 404 })
    );
}

/// A job action GitHub refuses for its rate limit keeps the status it answered; one runnerd holds back itself has
/// none, as it sent nothing.
#[tokio::test]
async fn a_rate_limit_answer_keeps_its_status_and_one_held_back_has_none() {
    let reset = (SystemTime::now() + Duration::from_secs(3600))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (url, seen) = fake_github(move |_| {
        (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "0".to_owned()),
                ("x-ratelimit-reset", reset.to_string()),
            ],
        )
            .into_response()
    })
    .await;
    let github = client(&url);
    let until = SystemTime::UNIX_EPOCH + Duration::from_secs(reset);
    let cancel = JobActionRequest::CancelRun { run: 10 };
    assert_eq!(
        github.job_action("o/r", cancel, &go).await,
        Err(ActionFailure::RateLimited {
            status: Some(403),
            until
        })
    );
    assert_eq!(
        github.job_action("o/r", cancel, &go).await,
        Err(ActionFailure::RateLimited {
            status: None,
            until
        }),
        "held back: nothing sent"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    let (url, _) =
        fake_github(|_| (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")]).into_response())
            .await;
    let refused = client(&url).job_action("o/r", cancel, &go).await;
    assert!(
        matches!(
            refused,
            Err(ActionFailure::RateLimited {
                status: Some(429),
                ..
            })
        ),
        "{refused:?}"
    );
}

/// A token source that counts its reads, and holds each from the `gated`th on until the test lets it go: `old` first,
/// `new` after.
fn gated_source(
    reads: &Arc<AtomicUsize>,
    gated: usize,
) -> (super::TokenSource, std::sync::mpsc::Sender<()>) {
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let reads = reads.clone();
    let source: super::TokenSource = Box::new(move || {
        let read = reads.fetch_add(1, Ordering::SeqCst) + 1;
        if read >= gated {
            let _ = gate.lock().unwrap().recv();
        }
        Ok(Token(if read == 1 { "old" } else { "new" }.to_owned()))
    });
    (source, release)
}

async fn reads_reach(reads: &AtomicUsize, count: usize) {
    while reads.load(Ordering::SeqCst) < count {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The permit is asked once the token is in hand and before each send, the one after a 401 included: a reload while
/// the token is read withdraws the action, and nothing is sent.
#[tokio::test]
async fn a_permit_that_says_no_once_the_token_is_read_sends_nothing() {
    let (url, seen) = fake_github(|_| StatusCode::ACCEPTED.into_response()).await;
    let reads = Arc::new(AtomicUsize::new(0));
    let (source, release) = gated_source(&reads, 1);
    let github = Arc::new(Github::new(&url, source).unwrap());
    let allowed = Arc::new(AtomicBool::new(true));
    let sending = {
        let (github, allowed) = (github.clone(), allowed.clone());
        tokio::spawn(async move {
            let permit = || -> BoxFuture<'_, bool> {
                let go = allowed.load(Ordering::SeqCst);
                Box::pin(async move { go })
            };
            github
                .job_action("o/r", JobActionRequest::CancelRun { run: 10 }, &permit)
                .await
        })
    };
    reads_reach(&reads, 1).await;
    allowed.store(false, Ordering::SeqCst);
    release.send(()).unwrap();
    assert_eq!(sending.await.unwrap(), Err(ActionFailure::Withdrawn));
    assert_eq!(seen.lock().unwrap().len(), 0, "nothing was sent");

    // A 401, and a reload while the new token is read: the second send does not go.
    let (url, seen) = fake_github(|request| {
        if request
            .headers
            .get("authorization")
            .map(HeaderValue::as_bytes)
            == Some(b"Bearer new")
        {
            StatusCode::ACCEPTED.into_response()
        } else {
            StatusCode::UNAUTHORIZED.into_response()
        }
    })
    .await;
    let reads = Arc::new(AtomicUsize::new(0));
    let (source, release) = gated_source(&reads, 2);
    let github = Arc::new(Github::new(&url, source).unwrap());
    let allowed = Arc::new(AtomicBool::new(true));
    let asked = Arc::new(AtomicUsize::new(0));
    let sending = {
        let (github, allowed, asked) = (github.clone(), allowed.clone(), asked.clone());
        tokio::spawn(async move {
            let permit = || -> BoxFuture<'_, bool> {
                asked.fetch_add(1, Ordering::SeqCst);
                let go = allowed.load(Ordering::SeqCst);
                Box::pin(async move { go })
            };
            github
                .job_action("o/r", JobActionRequest::CancelRun { run: 10 }, &permit)
                .await
        })
    };
    reads_reach(&reads, 2).await;
    allowed.store(false, Ordering::SeqCst);
    release.send(()).unwrap();
    assert_eq!(sending.await.unwrap(), Err(ActionFailure::Withdrawn));
    assert_eq!(asked.load(Ordering::SeqCst), 2, "asked before each send");
    assert_eq!(seen.lock().unwrap().len(), 1, "only the refused one went");
}

/// A token source that panics fails that read only: the next request reads again.
#[tokio::test]
async fn a_token_read_that_panics_fails_once_and_the_next_request_reads_again() {
    let (url, seen) = fake_github(|_| StatusCode::ACCEPTED.into_response()).await;
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    let source: super::TokenSource = Box::new(move || {
        assert!(
            counted.fetch_add(1, Ordering::SeqCst) > 0,
            "the first read panics"
        );
        Ok(Token("new".to_owned()))
    });
    let github = Github::new(&url, source).unwrap();
    let first = github
        .job_action("o/r", JobActionRequest::CancelRun { run: 1 }, &go)
        .await;
    assert!(first.is_err(), "no token came of the read that panicked");
    github
        .job_action("o/r", JobActionRequest::CancelRun { run: 1 }, &go)
        .await
        .unwrap();
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "the second request read again"
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "and only it reached GitHub");
}

/// A token read that hangs is read once, whoever stops waiting for it, and holds neither later requests' reads nor
/// the runtime's shutdown.
#[test]
fn a_token_read_that_hangs_is_read_once_and_holds_no_shutdown() {
    let reads = Arc::new(AtomicUsize::new(0));
    let (source, release) = gated_source(&reads, 1);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Never reached: no token comes.
        let github = Github::new("http://127.0.0.1:9", source).unwrap();
        for _ in 0..3 {
            let waited = tokio::time::timeout(
                Duration::from_millis(100),
                github.job_action("o/r", JobActionRequest::CancelRun { run: 1 }, &go),
            )
            .await;
            assert!(waited.is_err(), "still reading");
        }
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "later requests join the read"
        );
    });
    let (stopped, shut) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(runtime);
        let _ = stopped.send(());
    });
    assert!(
        shut.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the runtime stops with the read still hanging"
    );
    drop(release);
}

#[tokio::test]
async fn cancelling_a_run_that_already_completed_is_not_an_error() {
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs/10/cancel" => StatusCode::ACCEPTED.into_response(),
        "/repos/o/r/actions/runs/20/cancel" => (
            StatusCode::CONFLICT,
            r#"{"message":"Cannot cancel a workflow run that is completed."}"#,
        )
            .into_response(),
        _ => (
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible"}"#,
        )
            .into_response(),
    })
    .await;
    let github = client(&url);
    github.cancel_run("o/r", 10).await.unwrap();
    github.cancel_run("o/r", 20).await.unwrap();
    let error = github.cancel_run("o/r", 30).await.unwrap_err();
    assert!(
        error.to_string().contains("Resource not accessible"),
        "{error}"
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|seen| seen.method == Method::POST)
    );
}

#[tokio::test]
async fn a_jit_config_registers_the_runner_with_the_repository_labels() {
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

/// A repository's history as the fake GitHub's git sees it: each commit's parents, and its branches and tags.
struct History {
    parents: &'static [(&'static str, &'static [&'static str])],
    branches: &'static [(&'static str, &'static str)],
    tags: &'static [(&'static str, &'static str)],
}

impl History {
    /// What git takes a name in a compare for: a commit, else a tag, else a branch.
    fn resolve(&self, name: &str) -> Option<&'static str> {
        let known = |refs: &[(&str, &'static str)]| {
            refs.iter()
                .find(|(other, _)| *other == name)
                .map(|(_, sha)| *sha)
        };
        let commit = self.parents.iter().find(|(sha, _)| *sha == name);
        commit
            .map(|(sha, _)| *sha)
            .or_else(|| known(self.tags))
            .or_else(|| known(self.branches))
    }

    fn ancestor(&self, older: &str, newer: &str) -> bool {
        let parents = self
            .parents
            .iter()
            .find(|(sha, _)| *sha == newer)
            .map_or(&[][..], |(_, parents)| *parents);
        parents
            .iter()
            .any(|parent| *parent == older || self.ancestor(older, parent))
    }

    fn answer(&self, uri: &str) -> Response {
        let path = uri.trim_start_matches("/repos/o/r/");
        if let Some(branch) = path.strip_prefix("branches/") {
            return match self.branches.iter().find(|(name, _)| *name == branch) {
                Some((_, sha)) => json(&format!(r#"{{"commit":{{"sha":"{sha}"}}}}"#)),
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        let Some(range) = path
            .strip_prefix("compare/")
            .and_then(|rest| rest.strip_suffix("?per_page=1"))
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let Some((base, head)) = range.split_once("...") else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let (Some(base), Some(head)) = (self.resolve(base), self.resolve(head)) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let status = if base == head {
            "identical"
        } else if self.ancestor(head, base) {
            "behind"
        } else if self.ancestor(base, head) {
            "ahead"
        } else {
            "diverged"
        };
        json(&format!(r#"{{"status":"{status}"}}"#))
    }
}

#[tokio::test]
async fn a_commit_is_on_a_branch_only_as_its_head_or_an_ancestor() {
    // main is a0, a1, a2; someone tagged their own commit x9 `main`; release/1#1 forked at a0.
    let history = History {
        parents: &[
            ("a0", &[]),
            ("a1", &["a0"]),
            ("a2", &["a1"]),
            ("x9", &["a1"]),
            ("c1", &["a0"]),
        ],
        branches: &[("main", "a2"), ("release/1%231", "c1")],
        tags: &[("main", "x9")],
    };
    let (url, seen) = fake_github(move |request| history.answer(&request.uri)).await;
    let github = client(&url);
    assert!(
        github.on_branch("o/r", "main", "a2").await.unwrap(),
        "main's head"
    );
    assert!(
        github.on_branch("o/r", "main", "a0").await.unwrap(),
        "an ancestor"
    );
    assert!(
        !github.on_branch("o/r", "main", "x9").await.unwrap(),
        "the commit of a tag named main, which main never had"
    );
    assert!(
        github.on_branch("o/r", "release/1#1", "c1").await.unwrap(),
        "an escaped name"
    );
    assert!(
        !github.on_branch("o/r", "release/1#1", "a2").await.unwrap(),
        "diverged"
    );
    assert!(
        github.on_branch("o/r", "gone", "a2").await.is_err(),
        "no such branch"
    );
    assert!(
        github.on_branch("o/r", "main", "ee").await.is_err(),
        "unknown commit"
    );
    let asked = seen.lock().unwrap().len();
    assert!(
        !github.on_branch("o/r", "main", "../x").await.unwrap(),
        "not a commit"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        asked,
        "an invalid commit is never asked"
    );
}

#[tokio::test]
async fn a_run_and_its_pull_requests_say_whether_people_or_bots_started_them() {
    let person = r#"{"login":"owner","type":"User"}"#;
    let bot = r#"{"login":"renovate[bot]","type":"Bot"}"#;
    let (url, _) = fake_github(move |request| match request.uri.as_str() {
        "/repos/o/r/actions/runs/1" => json(&format!(
            r#"{{"event":"pull_request","head_branch":"feature","head_sha":"a1",
                "head_repository":{{"full_name":"o/r"}},"actor":{person},"triggering_actor":{person},
                "pull_requests":[
                    {{"number":7,"head":{{"ref":"feature","sha":"a1"}},
                      "base":{{"repo":{{"url":"https://api.github.com/repos/O/R"}}}}}},
                    {{"number":3,"head":{{"ref":"feature","sha":"a1"}},
                      "base":{{"repo":{{"url":"https://api.github.com/repos/upstream/r"}}}}}},
                    {{"number":4,"head":{{"ref":"feature","sha":"a1"}},
                      "base":{{"repo":{{"url":"https://api.github.com/repos/o/r2"}}}}}}
                ]}}"#
        )),
        // A GitHub App re-ran a run the owner's push started.
        "/repos/o/r/actions/runs/3" => json(&format!(
            r#"{{"event":"pull_request","head_branch":"feature","head_sha":"a1",
                "head_repository":{{"full_name":"o/r"}},"actor":{person},"triggering_actor":{bot},
                "pull_requests":[]}}"#
        )),
        // The owner re-ran a run Renovate's push started.
        "/repos/o/r/actions/runs/2" => json(&format!(
            r#"{{"event":"pull_request","head_branch":"renovate/x","head_sha":"b1",
                "head_repository":{{"full_name":"o/r"}},"actor":{bot},"triggering_actor":{person},
                "pull_requests":[]}}"#
        )),
        "/repos/o/r/pulls/7" => json(&format!(
            r#"{{"number":7,"user":{person},"head":{{"ref":"feature","sha":"a1","repo":{{"full_name":"o/r"}}}},
                "base":{{"ref":"main"}}}}"#
        )),
        // A bot's pull request from a fork that was deleted since.
        "/repos/o/r/pulls/8" => json(&format!(
            r#"{{"number":8,"user":{bot},"head":{{"ref":"patch-1","sha":"c1","repo":null}},
                "base":{{"ref":"main"}}}}"#
        )),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let github = client(&url);
    let run = github.run_trigger("o/r", 1).await.unwrap();
    assert!(run.by_user, "the owner pushed and started run 1");
    assert_eq!(
        run.pull_requests,
        [7],
        "pull requests into other repositories of the network are left out"
    );
    let rerun = github.run_trigger("o/r", 2).await.unwrap();
    assert!(!rerun.by_user, "Renovate started run 2");
    assert_eq!(
        rerun.pull_requests,
        Vec::<u64>::new(),
        "GitHub listed no pull request"
    );
    let app_rerun = github.run_trigger("o/r", 3).await.unwrap();
    assert!(!app_rerun.by_user, "a GitHub App re-ran run 3");
    let pull = github.pull_request("o/r", 7).await.unwrap();
    assert_eq!(
        pull,
        super::PullRequest {
            by_user: true,
            head_repository: Some("o/r".to_owned()),
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
        }
    );
    let fork = github.pull_request("o/r", 8).await.unwrap();
    assert!(!fork.by_user, "a bot opened pull request 8");
    assert_eq!(fork.head_repository, None);
    assert!(
        github.pull_request("o/r", 9).await.is_err(),
        "no such pull request"
    );
}

#[tokio::test]
async fn a_repository_says_its_default_branch_and_whether_it_is_public() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r" => {
            json(r#"{"default_branch":"trunk","private":true,"visibility":"internal"}"#)
        }
        "/repos/o/site" => {
            json(r#"{"default_branch":"main","private":false,"visibility":"public"}"#)
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let github = client(&url);
    let details = github.repository("o/r").await.unwrap();
    assert_eq!(
        (details.default_branch.as_str(), details.visibility),
        ("trunk", crate::api::Visibility::Internal)
    );
    assert_eq!(
        github.repository("o/site").await.unwrap().visibility,
        crate::api::Visibility::Public
    );
    assert!(
        github.repository("o/gone").await.is_err(),
        "a deleted repository is an error, not a visibility"
    );
}

/// A fake GitHub that tags every answer with the limit's reading, `remaining` lower with each one, and answers a
/// request conditional on the tag of its URI with a 304.
fn counting_fake() -> impl Fn(&Seen) -> Response + Send + Sync + 'static {
    let left = Arc::new(Mutex::new(5000_u64));
    move |request| {
        let remaining = {
            let mut left = left.lock().unwrap();
            *left -= 1;
            *left
        };
        let mut response = match (request.method.as_str(), request.uri.as_str()) {
            ("GET", "/repos/o/r/actions/runs?status=queued&per_page=100") => {
                page(request, r#"{"total_count":1,"workflow_runs":[{"id":10}]}"#, false)
            }
            ("GET", "/repos/o/r/actions/runs?status=in_progress&per_page=100") => {
                page(request, r#"{"total_count":0,"workflow_runs":[]}"#, false)
            }
            ("GET", "/repos/o/r/actions/runs/10/jobs?filter=latest&per_page=100") => {
                page(request, r#"{"jobs":[]}"#, false)
            }
            ("GET", "/repos/o/r/actions/runners?per_page=100") => {
                page(request, r#"{"runners":[]}"#, false)
            }
            ("GET", "/repos/o/r/actions/jobs/5") => page(
                request,
                r#"{"id":5,"run_id":10,"name":"smoke","status":"in_progress"}"#,
                false,
            ),
            ("GET", "/repos/o/r") => json(r#"{"default_branch":"main","visibility":"public"}"#),
            ("POST", "/repos/o/r/actions/runners/generate-jitconfig") => (
                StatusCode::CREATED,
                json(r#"{"runner":{"id":42,"name":"n","status":"offline","busy":false},"encoded_jit_config":"Yw=="}"#),
            )
                .into_response(),
            ("DELETE", "/repos/o/r/actions/runners/1") => StatusCode::NO_CONTENT.into_response(),
            ("DELETE", "/repos/o/r/actions/runners/2") => StatusCode::NOT_FOUND.into_response(),
            ("POST", "/repos/o/r/actions/runs/10/cancel") => StatusCode::FORBIDDEN.into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let headers = response.headers_mut();
        headers.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
        headers.insert(
            "x-ratelimit-remaining",
            HeaderValue::from_str(&remaining.to_string()).unwrap(),
        );
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("4000000000"));
        response
    }
}

/// How many requests of each class got a fresh answer, a 304 and a refusal, in `Class::ALL`'s order.
fn tallies(github: &Github) -> Vec<[u64; 3]> {
    let report = github.requests();
    Class::ALL
        .iter()
        .map(|class| {
            let tally = report.tally(*class);
            [tally.fresh, tally.unchanged, tally.refused]
        })
        .collect()
}

/// The bugs it catches: a class counted under another (a jobs page as a runs listing, a job read as `other`), a 304
/// counted as a fresh answer, a refusal not counted, and a request retried after a 401 counted once.
#[tokio::test]
async fn requests_are_counted_by_class_and_by_what_github_answered() {
    let (url, _) = fake_github(counting_fake()).await;
    let github = client(&url);
    // A poll of the repository, twice: the second poll's three answers are 304s.
    github.queued_jobs("o/r").await.unwrap();
    github.queued_jobs("o/r").await.unwrap();
    github.runners("o/r").await.unwrap();
    github.job("o/r", 5).await.unwrap();
    github.job("o/r", 5).await.unwrap();
    let labels = ["self-hosted".to_owned()];
    github.jit_config("o/r", "n", &labels).await.unwrap();
    github.delete_runner("o/r", 1).await.unwrap();
    assert_eq!(
        github.delete_runner("o/r", 2).await.unwrap(),
        Deleted::NotFound
    );
    github.repository("o/r").await.unwrap();
    assert!(github.cancel_run("o/r", 10).await.is_err());

    assert_eq!(
        tallies(&github),
        [
            [2, 2, 0], // runs: queued and in progress, twice
            [1, 1, 0], // jobs of run 10
            [1, 0, 0], // runners
            [1, 1, 0], // job reads
            [1, 0, 0], // registrations
            [2, 0, 0], // deletes
            [1, 0, 1], // other: the repository, the refused cancel
        ]
    );
    // What the pace is read by counts the registration and both deletes, whatever GitHub answered.
    assert_eq!(github.spend().mutations, 3);
}

#[tokio::test]
async fn a_request_the_token_was_refused_for_counts_each_time_it_was_sent() {
    let sent = Arc::new(Mutex::new(0));
    let counter = sent.clone();
    let (url, _) = fake_github(move |_| {
        let mut sent = counter.lock().unwrap();
        *sent += 1;
        if *sent == 1 {
            StatusCode::UNAUTHORIZED.into_response()
        } else {
            json(r#"{"runners":[]}"#)
        }
    })
    .await;
    let github = client(&url);
    github.runners("o/r").await.unwrap();
    assert_eq!(github.requests().tally(Class::Runners).total(), 2);
}

/// The bugs it catches: the limit read only from answers that carry a body, so a run of 304s leaves a stale count for
/// a governor to act on; and a reading of the search or GraphQL resource taken for the core one.
#[tokio::test]
async fn the_rate_limit_is_read_from_every_answer_of_the_core_resource() {
    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    let (url, _) = fake_github(move |request| {
        let call = {
            let mut calls = counter.lock().unwrap();
            *calls += 1;
            *calls
        };
        let mut response = if call == 3 {
            json(r#"{"runners":[]}"#)
        } else {
            page(request, r#"{"runners":[]}"#, false)
        };
        let headers = response.headers_mut();
        headers.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
        headers.insert(
            "x-ratelimit-remaining",
            HeaderValue::from_str(&(5000 - call * 10).to_string()).unwrap(),
        );
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("4000000000"));
        if call == 3 {
            headers.insert("x-ratelimit-resource", HeaderValue::from_static("search"));
        } else {
            headers.insert("x-ratelimit-resource", HeaderValue::from_static("core"));
        }
        response
    })
    .await;
    let github = client(&url);
    assert_eq!(github.requests().rate_limit(), None);
    github.runners("o/r").await.unwrap();
    github.runners("o/r").await.unwrap();
    // The second answer was a 304.
    let limit = github.requests().rate_limit().unwrap();
    assert_eq!(
        (limit.limit, limit.remaining, limit.reset),
        (
            5000,
            4980,
            SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000)
        )
    );
    github.runners("o/r").await.unwrap();
    assert_eq!(github.requests().rate_limit().unwrap().remaining, 4980);
}

fn no_headers() -> HeaderMap {
    HeaderMap::new()
}

/// The bugs it catches: a window that ends early or late, a new one that keeps the old one's counts, and the limit
/// lost with the window: the next window's line would say it was never read.
#[test]
fn a_window_reports_after_an_hour_and_the_next_starts_empty_with_the_limit_kept() {
    use super::requests::{Counter, WINDOW};
    let began = std::time::Instant::now();
    let mut counter = Counter::new(began, WINDOW);
    let mut headers = no_headers();
    headers.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("4200"));
    headers.insert("x-ratelimit-reset", HeaderValue::from_static("4000000000"));
    counter.record(Class::Runs, StatusCode::OK, &headers);
    counter.record(Class::Runs, StatusCode::NOT_MODIFIED, &no_headers());
    counter.record(Class::Delete, StatusCode::TOO_MANY_REQUESTS, &no_headers());
    counter.record(Class::Registration, StatusCode::OK, &no_headers());
    counter.record(Class::Other, StatusCode::OK, &no_headers());
    assert!(counter.due(began + Duration::from_secs(3599)).is_none());

    let report = counter.due(began + WINDOW).unwrap();
    assert_eq!(report.window(), WINDOW);
    assert_eq!(report.tally(Class::Runs).total(), 2);
    assert_eq!(report.tally(Class::Delete).refused, 1);
    assert_eq!(report.rate_limit().unwrap().remaining, 4200);
    // The registrations and deletes the pace reads go on counting across windows: a window's end is not a drop.
    assert_eq!(counter.spend().mutations, 2);

    counter.record(Class::Jobs, StatusCode::OK, &no_headers());
    let next = counter.peek(began + WINDOW + Duration::from_secs(5));
    assert_eq!(next.tally(Class::Runs).total(), 0);
    assert_eq!(next.tally(Class::Jobs).total(), 1);
    assert_eq!(next.window(), Duration::from_secs(5));
    assert_eq!(next.rate_limit().unwrap().remaining, 4200);
    assert_eq!(counter.spend().mutations, 2);
    assert!(
        counter
            .due(began + WINDOW + Duration::from_secs(3599))
            .is_none()
    );
}

/// The line the owner reads, in full: every class, a refusal only where there was one, and when the count starts
/// over. The bug it catches: a class missing from the line, or a figure under another's name.
#[test]
fn the_line_names_every_class_and_the_limit() {
    use super::requests::{Counter, WINDOW};
    let began = std::time::Instant::now();
    let mut counter = Counter::new(began, WINDOW);
    let mut headers = no_headers();
    headers.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("4200"));
    headers.insert("x-ratelimit-reset", HeaderValue::from_static("1800"));
    for (class, status) in [
        (Class::Runs, StatusCode::OK),
        (Class::Runs, StatusCode::NOT_MODIFIED),
        (Class::Runs, StatusCode::NOT_MODIFIED),
        (Class::Jobs, StatusCode::NOT_MODIFIED),
        (Class::Runners, StatusCode::OK),
        (Class::JobRead, StatusCode::OK),
        (Class::Registration, StatusCode::CREATED),
        (Class::Delete, StatusCode::NO_CONTENT),
        (Class::Other, StatusCode::FORBIDDEN),
    ] {
        counter.record(class, status, &headers);
    }
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1800 - 90);
    assert_eq!(
        counter.take(began + Duration::from_secs(3600)).line(now),
        "GitHub requests in the last 1h 00m: runs 3 (1 fresh, 2 unchanged), jobs 1 (0 fresh, 1 unchanged), \
         runners 1 (1 fresh, 0 unchanged), job reads 1 (1 fresh, 0 unchanged), registrations 1 (1 fresh, 0 unchanged), \
         deletes 1 (1 fresh, 0 unchanged), other 1 (0 fresh, 0 unchanged, 1 refused); \
         rate limit 4200 of 5000 left, resets in 1m 30s"
    );
}

/// The messages logged on this thread while installed with `tracing::subscriber::set_default`.
#[derive(Clone, Default)]
struct LogLines(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for LogLines {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Message<'lines>(&'lines Mutex<Vec<String>>);
        impl tracing::field::Visit for Message<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                if field.name() == "message" {
                    self.0.lock().unwrap().push(format!("{value:?}"));
                }
            }
        }
        event.record(&mut Message(&self.0));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// A client that asked GitHub something says so as it goes, and one that asked nothing stays quiet. The bug it
/// catches: the figures of the last window lost with the daemon, which is the window the e2e reads.
#[tokio::test]
async fn a_client_logs_what_it_asked_as_it_is_dropped() {
    // A second live dispatcher: with only one, `tracing-core` computes a callsite's interest from the thread that
    // reaches it first, and a parallel test with no subscriber caches "never".
    static SECOND: std::sync::LazyLock<tracing::Dispatch> =
        std::sync::LazyLock::new(|| tracing::Dispatch::new(LogLines::default()));
    std::sync::LazyLock::force(&SECOND);
    let lines = LogLines::default();
    let _installed = tracing::subscriber::set_default(lines.clone());
    let (url, _) = fake_github(counting_fake()).await;
    drop(client(&url));
    let ours = || -> Vec<String> {
        let lines = lines.0.lock().unwrap();
        lines
            .iter()
            .filter(|line| line.starts_with("GitHub requests"))
            .cloned()
            .collect()
    };
    assert!(ours().is_empty(), "nothing was asked");

    let github = client(&url);
    github.runners("o/r").await.unwrap();
    assert!(ours().is_empty(), "the hour is not over");
    drop(github);
    let logged = ours();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with("GitHub requests in the last 0s: runs 0 (0 fresh, 0 unchanged), jobs 0 (0 fresh, 0 unchanged), runners 1 (1 fresh, 0 unchanged)"),
        "{logged:?}"
    );
    assert!(!logged[0].contains(TOKEN));

    // A window that is over as the request is counted is logged with it, and the stop adds nothing for it.
    let github = client(&url);
    *lock(&github.requests) =
        super::requests::Counter::new(std::time::Instant::now(), Duration::ZERO);
    github.runners("o/r").await.unwrap();
    assert_eq!(ours().len(), 2, "{:?}", ours());
    drop(github);
    assert_eq!(
        ours().len(),
        2,
        "the stop repeated the hour's line: {:?}",
        ours()
    );
}

/// GitHub answers a token it refuses with the unauthenticated reading of the limit, 60 an hour. The bug it catches:
/// that reading replacing the account's, so a revoked token leaves a limit of 60 for a governor to pace by.
#[tokio::test]
async fn a_refused_token_does_not_change_the_rate_limit_reading() {
    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    let (url, _) = fake_github(move |_| {
        let call = {
            let mut calls = counter.lock().unwrap();
            *calls += 1;
            *calls
        };
        let (status, limit, remaining) = if call == 1 {
            (StatusCode::OK, "5000", "4990")
        } else {
            (StatusCode::UNAUTHORIZED, "60", "58")
        };
        (
            status,
            [
                ("x-ratelimit-limit", limit),
                ("x-ratelimit-remaining", remaining),
                ("x-ratelimit-reset", "4000000000"),
            ],
            r#"{"runners":[]}"#,
        )
            .into_response()
    })
    .await;
    let github = client(&url);
    github.runners("o/r").await.unwrap();
    assert!(github.runners("o/r").await.is_err());
    let report = github.requests();
    assert_eq!(
        report.tally(Class::Runners).total(),
        3,
        "both sends of the refused request count"
    );
    assert_eq!(report.rate_limit().unwrap().limit, 5000);
}
