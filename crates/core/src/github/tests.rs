//! The GitHub client against a fake GitHub on a loopback port: queued jobs, listings over several pages,
//! conditional requests and how much of them is kept, rate limits, token refresh, runner registration and deletion, who started a run and opened a pull request, and that the token
//! never shows in `Debug` output or errors.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;

use super::workflows::{NAMES_FRESH, Workflows};
use super::{
    ActionFailure, Class, Conditional, Deleted, Error, Github, GithubApi, JobActionRequest,
    NamedRun, QueuedJob, RunNow, Token,
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

/// Where the client reads the repository's workflows. A fake GitHub whose handler does not answer it answers one
/// workflow, 7, `CI`.
const WORKFLOWS: &str = "/repos/o/r/actions/workflows?per_page=100";

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
        let response = if request.uri == WORKFLOWS && response.status() == StatusCode::NOT_FOUND {
            json(r#"{"total_count":1,"workflows":[{"id":7,"name":"CI","path":".github/workflows/ci.yml"}]}"#)
        } else {
            response
        };
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

/// The queued jobs of a listing that follows no job.
async fn queued(github: &Github, repository: &str) -> Result<Vec<QueuedJob>, Error> {
    github
        .queued_jobs(repository, &std::collections::BTreeSet::new())
        .await
        .map(|listing| listing.queued)
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
    let jobs = queued(&client(&url), "o/r").await.unwrap();
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
            json(
                r#"{"runners":[{"id":7,"name":"runnerd-a","status":"online","busy":true,"labels":[
                    {"id":1,"name":"self-hosted","type":"read-only"},{"id":9,"name":"app","type":"custom"}]}]}"#,
            ),
        )
            .into_response()
    })
    .await;
    let github = client(&url);
    let first = github.runners("o/r").await.unwrap();
    let second = github.runners("o/r").await.unwrap();
    assert_eq!(first, second);
    assert_eq!(second[0].id, 7);
    assert_eq!(
        second[0].labels,
        ["self-hosted", "app"],
        "the labels' names"
    );
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
            page(request, r#"{"workflow_runs":[{"id":10,"workflow_id":7}]}"#, true)
        }
        // Run 10 moved down a page while the first was read.
        "/repos/o/r/actions/runs?status=queued&per_page=100&page=2" => {
            page(request, r#"{"workflow_runs":[{"id":10,"workflow_id":7},{"id":20,"workflow_id":7}]}"#, false)
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
    let first = queued(&github, "o/r").await.unwrap();
    let sent = seen.lock().unwrap().len();
    let second = queued(&github, "o/r").await.unwrap();
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
        .map(|id| format!(r#"{{"id":{id},"workflow_id":7}}"#))
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
    let jobs = queued(&client(&url), "o/r").await.unwrap();
    assert_eq!(
        jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
        (1..=50).chain(301..=450).collect::<Vec<_>>(),
        "every queued run, then the oldest runs in progress up to 200 runs, the last read to its end"
    );
    // Five pages of runs, 200 runs' jobs, run 50's second page, and the workflows' names once.
    assert_eq!(seen.lock().unwrap().len(), 207);
}

/// Serves job `id` as completed when `id` is in `completed`, otherwise in progress with a name `pad` long.
async fn fake_jobs(
    completed: std::ops::RangeInclusive<u64>,
    pad: usize,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_github(move |request| {
        if request.uri == "/repos/o/r/actions/runs/1" {
            return json(r#"{"id":1,"workflow_id":7}"#);
        }
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
        queued(&github, "o/r").await,
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
async fn a_rejected_token_is_read_again_once_and_both_sends_count_as_requests() {
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
    let sent = crate::github::REQUESTS
        .scope(std::cell::Cell::new(0), async {
            assert_eq!(github.runners("o/r").await.unwrap(), []);
            crate::github::requests_sent()
        })
        .await;
    assert_eq!(*reads.lock().unwrap(), 2);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(
        sent, 2,
        "the send after the 401 counts against a refresh's budget"
    );
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
            json(r#"{"id":10,"run_attempt":3,"status":"completed","conclusion":"failure","event":"push","head_sha":"a"}"#)
        }
        "/repos/o/r/actions/runs/11" => {
            json(r#"{"id":11,"status":"in_progress","event":"push","head_sha":"a"}"#)
        }
        "/repos/o/r/actions/runs/13" => json(
            r#"{"id":13,"run_attempt":1,"status":"completed","conclusion":"cancelled","event":"push","head_sha":"a"}"#,
        ),
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
            completed: true,
            failed: true,
        })
    );
    assert_eq!(
        github.action_run("o/r", 11).await,
        Ok(RunNow {
            attempt: 1,
            completed: false,
            failed: false,
        })
    );
    // Cancelled as a whole, whatever its jobs say.
    assert_eq!(
        github.action_run("o/r", 13).await,
        Ok(RunNow {
            attempt: 1,
            completed: true,
            failed: false,
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
        queued(&github, "o/r").await.unwrap();
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
                page(request, r#"{"total_count":1,"workflow_runs":[{"id":10,"workflow_id":7}]}"#, false)
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

/// How many requests of each class asked at all got a fresh answer, a 304 and a refusal, in `Class::ALL`'s order.
fn tallies(github: &Github) -> Vec<(Class, [u64; 3])> {
    let report = github.requests();
    Class::ALL
        .iter()
        .map(|class| (*class, report.tally(*class)))
        .filter(|(_, tally)| tally.total() > 0)
        .map(|(class, tally)| (class, [tally.fresh, tally.unchanged, tally.refused]))
        .collect()
}

/// The bugs it catches: a class counted under another (a jobs page as a runs listing, a job read as `other`), a 304
/// counted as a fresh answer, a refusal not counted, and a request retried after a 401 counted once.
#[tokio::test]
async fn requests_are_counted_by_class_and_by_what_github_answered() {
    let (url, _) = fake_github(counting_fake()).await;
    let github = client(&url);
    // A poll of the repository, twice: the second poll's three answers are 304s.
    queued(&github, "o/r").await.unwrap();
    queued(&github, "o/r").await.unwrap();
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
            (Class::Runs, [2, 2, 0]), // queued and in progress, twice
            (Class::Jobs, [1, 1, 0]), // of run 10
            (Class::Runners, [1, 0, 0]),
            (Class::JobRead, [1, 1, 0]),
            (Class::Registration, [1, 0, 0]),
            (Class::Delete, [2, 0, 0]),
            (Class::Repository, [1, 0, 0]),
            (Class::Workflows, [1, 0, 0]), // the workflows' names, for job 5's run
            (Class::Cancel, [0, 0, 1]),
        ]
    );
    let other = github.requests().other();
    assert_eq!(
        (other.fresh, other.refused),
        (2, 1),
        "other sums its classes"
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
        (Class::Trigger, StatusCode::OK),
        (Class::Trigger, StatusCode::NOT_MODIFIED),
        (Class::Cancel, StatusCode::FORBIDDEN),
    ] {
        counter.record(class, status, &headers);
    }
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1800 - 90);
    assert_eq!(
        counter.take(began + Duration::from_secs(3600)).line(now),
        "GitHub requests in the last 1h 00m: runs 3 (1 fresh, 2 unchanged), jobs 1 (0 fresh, 1 unchanged), \
         runners 1 (1 fresh, 0 unchanged), job reads 1 (1 fresh, 0 unchanged), registrations 1 (1 fresh, 0 unchanged), \
         deletes 1 (1 fresh, 0 unchanged), other 3 (1 fresh, 1 unchanged, 1 refused) \
         [run triggers 2 (1 fresh, 1 unchanged), cancels 1 (0 fresh, 0 unchanged, 1 refused)]; \
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

/// Job `id` of run `run`, named by GitHub after the run's `run-name` ("deploy {run}"), on `main`, completed with
/// `conclusion` by runner `r{id}`.
fn run_named_job(id: u64, run: u64, conclusion: &str) -> String {
    format!(
        r#"{{"id":{id},"run_id":{run},"run_attempt":1,"name":"test","workflow_name":"deploy {run}",
        "head_branch":"main","labels":["self-hosted"],"created_at":"2026-11-14T10:0{id}:00Z","status":"completed",
        "conclusion":"{conclusion}","started_at":"2026-11-14T10:0{id}:10Z","completed_at":"2026-11-14T10:0{id}:50Z",
        "runner_name":"r{id}","steps":[]}}"#
    )
}

/// Two runs of workflow 7 under different `run-name`s, and a run of workflow 8, which the repository's workflows no
/// longer list.
async fn run_named_github() -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/jobs/1" => json(&run_named_job(1, 100, "success")),
        "/repos/o/r/actions/jobs/2" => json(&run_named_job(2, 200, "failure")),
        "/repos/o/r/actions/jobs/3" => json(&run_named_job(3, 200, "success")),
        "/repos/o/r/actions/jobs/4" => json(&run_named_job(4, 300, "success")),
        "/repos/o/r/actions/runs/100" | "/repos/o/r/actions/runs/200" => {
            json(r#"{"workflow_id":7,"name":"deploy","display_title":"deploy"}"#)
        }
        "/repos/o/r/actions/runs/300" => json(r#"{"workflow_id":8}"#),
        "/repos/o/r/actions/runs?status=queued&per_page=100" => {
            json(r#"{"workflow_runs":[{"id":200,"workflow_id":7}]}"#)
        }
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            json(r#"{"workflow_runs":[]}"#)
        }
        "/repos/o/r/actions/runs/200/jobs?filter=latest&per_page=100" => json(
            r#"{"jobs":[{"id":5,"run_id":200,"name":"lint","status":"queued","labels":[],"workflow_name":"deploy 200"}]}"#,
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await
}

/// The bug it catches: GitHub's `workflow_name` taken for the job's workflow, which is the run's name, so a workflow
/// with a `run-name` is a new workflow on every run and compare finds no earlier success of the same job.
#[tokio::test]
async fn two_runs_of_one_workflow_under_different_run_names_are_one_job_and_compare_finds_the_baseline()
 {
    let (url, _) = run_named_github().await;
    let github = client(&url);
    let passed = github.job("o/r", 1).await.unwrap();
    let failed = github.job("o/r", 2).await.unwrap();

    let dir = crate::testdir::TestDir::new("github");
    let store = crate::store::Store::open(&dir.0.join("runnerd.db")).unwrap();
    for (details, at) in [(passed, 1_000_000), (failed, 2_000_000)] {
        let runner = details.runner_name.clone().unwrap();
        store.runner_started(crate::store::RunnerRecord {
            purpose: None,
            admission: None,
            name: runner.clone(),
            repository: "o/r".to_owned(),
            profile: "app".to_owned(),
            slot: 1,
            container: format!("{runner}-container"),
            started_at: at,
            memory_cap: None,
            cpu_cap: None,
        });
        store.claimed(details.id, details.run_id, &runner, at + 100);
        store.details(details);
        store.runner_removed(&runner, at + 60_000);
    }
    store.flush().await;
    let compared = store.compare(2, None).await.unwrap().unwrap();
    assert!(
        matches!(
            compared.baseline,
            crate::api::Baseline::Chosen { job: 1, .. }
        ),
        "job 1, the earlier success of `CI / test` on main, is the baseline: {:?}",
        compared.baseline
    );
    let job = store.job(2).await.unwrap().unwrap();
    assert_eq!(
        job.summary.workflow.as_deref(),
        Some("CI"),
        "the workflow's own name, not the run's"
    );
}

/// The bugs it catches: a request for the run or the workflows on every job read, a client that does not learn them
/// again after a restart, a queued job keeping the run's name, and a workflow the listing lacks named wrongly.
#[tokio::test]
async fn a_jobs_workflow_is_asked_once_per_run_learnt_again_after_a_restart_and_unknown_keeps_githubs_name()
 {
    let (url, seen) = run_named_github().await;
    let asked = |uri: &str| uris(&seen).iter().filter(|seen| *seen == uri).count();
    let github = client(&url);
    let queued = queued(&github, "o/r").await.unwrap();
    assert_eq!(queued[0].workflow.as_deref(), Some("CI"));
    for id in [2, 3] {
        assert_eq!(
            github
                .job("o/r", id)
                .await
                .unwrap()
                .workflow_name
                .as_deref(),
            Some("CI")
        );
    }
    assert_eq!(
        asked("/repos/o/r/actions/runs/200"),
        0,
        "the listing gave run 200's workflow"
    );
    assert_eq!(asked(WORKFLOWS), 1);
    github.job("o/r", 1).await.unwrap();
    assert_eq!(asked("/repos/o/r/actions/runs/100"), 1);
    assert_eq!(asked(WORKFLOWS), 1, "workflow 7's name is known");

    // A restart: nothing kept, everything learnt again.
    let restarted = client(&url);
    assert_eq!(
        restarted
            .job("o/r", 3)
            .await
            .unwrap()
            .workflow_name
            .as_deref(),
        Some("CI")
    );
    assert_eq!(asked("/repos/o/r/actions/runs/200"), 1);
    assert_eq!(asked(WORKFLOWS), 2);

    // Workflow 8 is not listed, even read again: GitHub's own name stays, as before.
    assert_eq!(
        restarted
            .job("o/r", 4)
            .await
            .unwrap()
            .workflow_name
            .as_deref(),
        Some("deploy 300")
    );
    assert_eq!(
        asked(WORKFLOWS),
        3,
        "a workflow missing from the list reads it again"
    );
    restarted.job("o/r", 4).await.unwrap();
    assert_eq!(asked(WORKFLOWS), 3, "once per workflow it does not list");
}

/// Run 100's job 1, whose workflow's name GitHub answers only while `listing` holds, and run 404's job 2, a run GitHub
/// no longer has.
async fn flaky_workflows(listing: Arc<AtomicBool>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_github(move |request| match request.uri.as_str() {
        "/repos/o/r/actions/jobs/1" => json(&run_named_job(1, 100, "success")),
        "/repos/o/r/actions/jobs/2" => json(&run_named_job(2, 404, "success")),
        "/repos/o/r/actions/jobs/3" => json(&run_named_job(3, 300, "success")),
        "/repos/o/r/actions/runs/100" | "/repos/o/r/actions/runs/300" => {
            json(r#"{"workflow_id":7}"#)
        }
        WORKFLOWS if !listing.load(Ordering::SeqCst) => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await
}

/// The bugs it catches: a job read failing because its workflow's name could not be read, which the history then
/// records as lost or gone though GitHub answered for the job, and a run whose name came late never named.
#[tokio::test]
async fn a_job_whose_workflow_cannot_be_read_keeps_githubs_name_and_its_run_is_named_once_it_can() {
    let listing = Arc::new(AtomicBool::new(false));
    let (url, _) = flaky_workflows(listing.clone()).await;
    let mut github = client(&url);
    *github.workflows.get_mut().unwrap() = Workflows::new(NAMES_FRESH, Duration::ZERO);
    let job = github.job("o/r", 1).await.unwrap();
    assert_eq!(
        job.workflow_name.as_deref(),
        Some("deploy 100"),
        "GitHub's name, for now"
    );
    assert_eq!(job.conclusion.as_deref(), Some("success"));
    assert_eq!(github.named_runs().await, [], "the workflows still fail");

    listing.store(true, Ordering::SeqCst);
    assert_eq!(
        github.named_runs().await,
        [NamedRun {
            repository: "o/r".to_owned(),
            run_id: 100,
            workflow: "CI".to_owned(),
        }]
    );
    assert_eq!(github.named_runs().await, [], "given once");

    // A run GitHub no longer has: the job is still read, under GitHub's name, and nothing is owed.
    let job = github.job("o/r", 2).await.unwrap();
    assert_eq!(job.workflow_name.as_deref(), Some("deploy 404"));
    assert_eq!(github.named_runs().await, []);
}

/// The bug it catches: a failing read of the workflows asked again by every job read of every tick.
#[tokio::test]
async fn after_a_failed_read_of_the_workflows_job_reads_ask_nothing_more_for_a_while() {
    let (url, seen) = flaky_workflows(Arc::new(AtomicBool::new(false))).await;
    let github = client(&url);
    for id in [1, 3, 1] {
        assert!(github.job("o/r", id).await.is_ok(), "job {id} is read");
    }
    assert_eq!(github.named_runs().await, []);
    let asked = |uri: &str| uris(&seen).iter().filter(|seen| *seen == uri).count();
    assert_eq!(asked(WORKFLOWS), 1);
    assert_eq!(
        asked("/repos/o/r/actions/runs/300"),
        0,
        "nothing read for a minute"
    );
}

/// The bug it catches: job reads at once, as the history makes them, each reading the run and the workflows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_reads_at_once_read_their_run_and_the_workflows_once() {
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs/100" => {
            std::thread::sleep(Duration::from_millis(100));
            json(r#"{"workflow_id":7}"#)
        }
        uri => match uri.strip_prefix("/repos/o/r/actions/jobs/") {
            Some(id) => json(&run_named_job(id.parse().unwrap(), 100, "success")),
            None => StatusCode::NOT_FOUND.into_response(),
        },
    })
    .await;
    let github = client(&url);
    let reads = futures_util::future::join_all((1..=4).map(|id| github.job("o/r", id))).await;
    for read in reads {
        assert_eq!(read.unwrap().workflow_name.as_deref(), Some("CI"));
    }
    let asked = |uri: &str| uris(&seen).iter().filter(|seen| *seen == uri).count();
    assert_eq!(asked("/repos/o/r/actions/runs/100"), 1);
    assert_eq!(asked(WORKFLOWS), 1);
}

/// The bug it catches: a workflow's name read once for the daemon's life, so a rename shows only at a restart and
/// splits the history there.
#[tokio::test]
async fn a_renamed_workflow_takes_its_new_name_once_the_names_are_old() {
    let name = Arc::new(Mutex::new("CI"));
    let listed = name.clone();
    let (url, _) = fake_github(move |request| match request.uri.as_str() {
        "/repos/o/r/actions/jobs/1" => json(&run_named_job(1, 100, "success")),
        "/repos/o/r/actions/jobs/3" => json(&run_named_job(3, 300, "success")),
        "/repos/o/r/actions/runs/100" | "/repos/o/r/actions/runs/300" => {
            json(r#"{"workflow_id":7}"#)
        }
        WORKFLOWS => json(&format!(
            r#"{{"workflows":[{{"id":7,"name":"{}"}}]}}"#,
            listed.lock().unwrap()
        )),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let mut github = client(&url);
    *github.workflows.get_mut().unwrap() = Workflows::new(Duration::ZERO, Duration::ZERO);
    let workflow = async |id| github.job("o/r", id).await.unwrap().workflow_name;
    assert_eq!(workflow(1).await.as_deref(), Some("CI"));
    *name.lock().unwrap() = "Build";
    assert_eq!(workflow(3).await.as_deref(), Some("Build"));
}

/// An action's archive is downloaded anonymously from codeload: no request carries the token, a refusal is not asked
/// again with it, a redirect is not followed, and no error says the token or an address's query.
#[tokio::test]
async fn an_archive_is_downloaded_without_the_token_and_never_asked_again_with_it() {
    use crate::archives::Source as _;
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        "/actions/checkout/tar.gz/abc" => b"archive bytes".to_vec().into_response(),
        "/o/private/tar.gz/abc" => StatusCode::NOT_FOUND.into_response(),
        "/o/moved/tar.gz/abc" => (
            StatusCode::FOUND,
            [("location", "/elsewhere/a.tar.gz?token=SIGNATURE")],
        )
            .into_response(),
        "/o/big/tar.gz/abc" => axum::body::Body::from_stream(futures_util::stream::iter(
            [b"archive ".as_slice(), b"bytes"]
                .map(|part| Ok::<_, std::io::Error>(axum::body::Bytes::from_static(part))),
        ))
        .into_response(),
        "/o/cut/tar.gz/abc" => axum::body::Body::from_stream(futures_util::stream::iter([
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"part of it")),
            Err(std::io::Error::other("connection reset")),
        ]))
        .into_response(),
        _ => StatusCode::UNAUTHORIZED.into_response(),
    })
    .await;
    let github = client(&url).with_codeload(&url);
    let dir = crate::testdir::TestDir::new("tarball");

    let to = dir.0.join("a");
    let ignored = AtomicU64::new(0);
    let written = github
        .tarball("actions/checkout", "abc", &to, 1024, &ignored)
        .await;
    assert_eq!(written.ok(), Some(13), "the archive is written whole");
    assert_eq!(std::fs::read(&to).unwrap(), b"archive bytes");
    let private = github
        .tarball("o/private", "abc", &dir.0.join("b"), 1024, &ignored)
        .await
        .unwrap_err();
    assert!(
        private.is_gone(),
        "a private repository is not there anonymously"
    );
    let refused = github
        .tarball("o/refused", "abc", &dir.0.join("c"), 1024, &ignored)
        .await;
    assert!(
        refused.is_err(),
        "a 401 is a failure, never a cue to send the token"
    );
    let received = AtomicU64::new(0);
    let capped = github
        .tarball("o/big", "abc", &dir.0.join("d"), 4, &received)
        .await
        .unwrap_err();
    assert!(
        received.load(Ordering::Relaxed) > 4,
        "a streamed chunk past the cap counts, though it was never written"
    );
    let moved = github
        .tarball("o/moved", "abc", &dir.0.join("e"), 1024, &ignored)
        .await
        .unwrap_err();
    let cut = github
        .tarball("o/cut", "abc", &dir.0.join("f"), 1024, &ignored)
        .await
        .unwrap_err();
    let requests = lock(&seen).clone();
    assert!(
        requests
            .iter()
            .all(|request| !request.headers.contains_key("authorization")),
        "no archive request carries a token"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.uri == "/o/refused/tar.gz/abc")
            .count(),
        1,
        "a refusal is not asked again"
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.uri.contains("elsewhere")),
        "a redirect is not followed"
    );
    for error in [private, capped, moved, cut] {
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains(TOKEN),
            "an archive's error never shows the token"
        );
        assert!(
            !shown.contains("SIGNATURE"),
            "an archive's error never shows an address's query"
        );
    }
}

/// An attempt's jobs are all of them or none: a listing cut at the page cap, or one with fewer jobs than GitHub counts,
/// is a failed read, as a job left unread could be one that failed in its code.
#[tokio::test]
async fn an_attempts_jobs_cut_short_or_uncounted_are_a_failed_read() {
    let (url, _) = fake_github(|request| {
        if request.uri.starts_with("/repos/o/r/actions/runs/5/attempts/1/jobs") {
            // Every page links a next one, past the cap.
            let id = request.uri.len();
            return page(
                request,
                &format!(
                    r#"{{"total_count":2000,"jobs":[{{"id":{id},"status":"completed","conclusion":"failure"}}]}}"#
                ),
                true,
            );
        }
        if request.uri.starts_with("/repos/o/r/actions/runs/6/attempts/1/jobs") {
            return page(
                request,
                r#"{"total_count":3,"jobs":[{"id":1,"status":"completed","conclusion":"failure"}]}"#,
                false,
            );
        }
        if request.uri.starts_with("/repos/o/r/actions/runs/7/attempts/1/jobs") {
            return page(
                request,
                r#"{"total_count":1,"jobs":[{"id":1,"status":"completed","conclusion":"failure"}]}"#,
                false,
            );
        }
        // No count and no next page: the jobs shown may be a few of many.
        if request.uri.starts_with("/repos/o/r/actions/runs/8/attempts/1/jobs") {
            return page(
                request,
                r#"{"jobs":[{"id":1,"status":"completed","conclusion":"failure"}]}"#,
                false,
            );
        }
        StatusCode::NOT_FOUND.into_response()
    })
    .await;
    let github = client(&url);
    assert!(
        github.attempt_jobs("o/r", 5, 1).await.is_err(),
        "a listing past the page cap is refused"
    );
    assert!(
        github.attempt_jobs("o/r", 6, 1).await.is_err(),
        "a listing short of its total_count is refused"
    );
    assert!(
        github.attempt_jobs("o/r", 8, 1).await.is_err(),
        "a listing without a total_count is refused"
    );
    assert_eq!(
        github
            .attempt_jobs("o/r", 7, 1)
            .await
            .map(|jobs| jobs.len()),
        Ok(1)
    );
}

/// The build cache's trust reads as a fake GitHub answers them, which a test changes between reads: each path's
/// status and body, its `ETag` drawn from the body, and a 304 to a request conditional on the current one.
#[derive(Clone, Default)]
struct TrustReads(Arc<Mutex<std::collections::HashMap<String, (StatusCode, String)>>>);

/// Who started a run: a person, or a GitHub App.
const PERSON: &str = r#"{"login":"owner","type":"User"}"#;
const BOT: &str = r#"{"login":"renovate[bot]","type":"Bot"}"#;

/// Run 1's body: a push to main of `a1`, its actor a person and its triggering actor `triggering`.
fn trusted_run(triggering: &str) -> String {
    format!(
        r#"{{"workflow_id":7,"event":"push","head_branch":"main","head_sha":"a1",
            "head_repository":{{"full_name":"o/r"}},"actor":{PERSON},"triggering_actor":{triggering},
            "pull_requests":[]}}"#
    )
}

/// Pull request 7's body: a person's, from `head` of `head_repo`, into `base`.
fn trusted_pull(head_repo: &str, head: &str, base: &str) -> String {
    format!(
        r#"{{"number":7,"user":{PERSON},"head":{{"ref":"{head}","sha":"a1","repo":{{"full_name":"{head_repo}"}}}},
            "base":{{"ref":"{base}"}}}}"#
    )
}

const RUN: &str = "/repos/o/r/actions/runs/1";
const PULL: &str = "/repos/o/r/pulls/7";
const BRANCH: &str = "/repos/o/r/branches/main";

/// Where the client compares `head`, the branch's head, with the run's commit `a1`.
fn compare(head: &str) -> String {
    format!("/repos/o/r/compare/{head}...a1?per_page=1")
}

/// A body's `ETag`, which changes with it.
fn etag_of(body: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:x}\"", hasher.finish())
}

impl TrustReads {
    /// Run 1, pushed to main by a person; pull request 7 from `feature` into main; main at `a2`, a1's child.
    fn new() -> TrustReads {
        let reads = TrustReads::default();
        reads.answer(RUN, StatusCode::OK, &trusted_run(PERSON));
        reads.answer(
            PULL,
            StatusCode::OK,
            &trusted_pull("o/r", "feature", "main"),
        );
        reads.answer(BRANCH, StatusCode::OK, r#"{"commit":{"sha":"a2"}}"#);
        reads.answer(&compare("a2"), StatusCode::OK, r#"{"status":"behind"}"#);
        reads
    }

    fn answer(&self, uri: &str, status: StatusCode, body: &str) {
        self.0
            .lock()
            .unwrap()
            .insert(uri.to_owned(), (status, body.to_owned()));
    }

    /// Answers `request`; a body `no-etag:` starts is sent without its prefix and without an `ETag`, and one `next:`
    /// starts without its prefix and with a `Link` to a next page.
    fn respond(&self, request: &Seen) -> Response {
        let Some((status, body)) = self.0.lock().unwrap().get(&request.uri).cloned() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if let Some(body) = body.strip_prefix("next:") {
            let mut response = TrustReads::respond_with(request, status, body);
            response.headers_mut().insert(
                "link",
                HeaderValue::from_static(r#"<https://api.github.com/next>; rel="next""#),
            );
            return response;
        }
        TrustReads::respond_with(request, status, &body)
    }

    fn respond_with(request: &Seen, status: StatusCode, body: &str) -> Response {
        if let Some(body) = body.strip_prefix("no-etag:") {
            return (status, json(body)).into_response();
        }
        if status != StatusCode::OK {
            return (status, json(body)).into_response();
        }
        let etag = etag_of(body);
        if request
            .headers
            .get("if-none-match")
            .is_some_and(|sent| sent.as_bytes() == etag.as_bytes())
        {
            return (StatusCode::NOT_MODIFIED, [("etag", etag)]).into_response();
        }
        ([("etag", etag)], json(body)).into_response()
    }
}

async fn trust_github(reads: &TrustReads) -> (Github, Arc<Mutex<Vec<Seen>>>) {
    let answering = reads.clone();
    let (url, seen) = fake_github(move |request| answering.respond(request)).await;
    (client(&url), seen)
}

/// How many requests of `class` got a fresh answer, a 304 and a refusal.
fn tally_of(github: &Github, class: Class) -> [u64; 3] {
    let tally = github.requests().tally(class);
    [tally.fresh, tally.unchanged, tally.refused]
}

/// Every job of a run asks the same four things of GitHub before it may write. The bug it catches: the answers
/// dropped after each read, so each job pays the rate limit for a run, a pull request, a branch and a compare that
/// did not change; and a 304 that answers anything but what the first read said.
#[tokio::test]
async fn the_build_caches_reads_of_an_unchanged_run_are_304s_that_answer_as_before() {
    let reads = TrustReads::new();
    let (github, seen) = trust_github(&reads).await;
    let first = (
        github.run_trigger("o/r", 1).await.unwrap(),
        github.pull_request("o/r", 7).await.unwrap(),
        github.on_branch("o/r", "main", "a1").await.unwrap(),
    );
    for _ in 0..2 {
        let again = (
            github.run_trigger("o/r", 1).await.unwrap(),
            github.pull_request("o/r", 7).await.unwrap(),
            github.on_branch("o/r", "main", "a1").await.unwrap(),
        );
        assert_eq!(again, first, "a 304 answers what the first read said");
    }
    assert!(first.0.by_user && first.2, "a person's push, on main");
    for uri in [RUN, PULL, BRANCH, &compare("a2")] {
        assert_eq!(
            conditional(&seen, uri),
            [false, true, true],
            "{uri} is asked again, conditionally"
        );
    }
    for class in [
        Class::Trigger,
        Class::PullRequest,
        Class::Branch,
        Class::Compare,
    ] {
        assert_eq!(tally_of(&github, class), [1, 2, 0], "{class:?}");
    }
    assert_eq!(
        github.requests().other().fresh,
        4,
        "only the first reads cost"
    );
}

/// Every decision asks GitHub again, and a changed answer is the one that counts. The bugs it catches: a run's
/// trigger, a pull request's head or base, or a branch's head taken from what was kept once GitHub said something
/// else; a re-run by a bot allowed because the run's ID was allowed before; a deleted branch or a failed compare
/// answered from the last good read.
#[tokio::test]
async fn a_changed_trust_answer_replaces_the_kept_one_and_a_failed_one_is_an_error() {
    let reads = TrustReads::new();
    let (github, seen) = trust_github(&reads).await;
    assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
    github.pull_request("o/r", 7).await.unwrap();
    assert!(github.on_branch("o/r", "main", "a1").await.unwrap());

    // A GitHub App re-runs the run: same ID, another triggering actor.
    reads.answer(RUN, StatusCode::OK, &trusted_run(BOT));
    assert!(
        !github.run_trigger("o/r", 1).await.unwrap().by_user,
        "the bot's re-run"
    );
    // The pull request's head moves to a fork, and its base to release.
    reads.answer(
        PULL,
        StatusCode::OK,
        &trusted_pull("f/r", "patch", "release"),
    );
    assert_eq!(
        github.pull_request("o/r", 7).await.unwrap(),
        super::PullRequest {
            by_user: true,
            head_repository: Some("f/r".to_owned()),
            head_branch: "patch".to_owned(),
            base_branch: "release".to_owned(),
        }
    );
    // main is force-moved to a commit without a1.
    reads.answer(BRANCH, StatusCode::OK, r#"{"commit":{"sha":"c9"}}"#);
    reads.answer(&compare("c9"), StatusCode::OK, r#"{"status":"diverged"}"#);
    assert!(
        !github.on_branch("o/r", "main", "a1").await.unwrap(),
        "a1 is no longer on main"
    );
    assert_eq!(
        conditional(&seen, RUN),
        [false, true],
        "the re-run was asked conditionally, and GitHub's new answer taken"
    );
    // The compare fails, then main is deleted.
    reads.answer(
        &compare("c9"),
        StatusCode::NOT_FOUND,
        r#"{"message":"No common ancestor"}"#,
    );
    assert!(github.on_branch("o/r", "main", "a1").await.is_err());
    reads.answer(
        BRANCH,
        StatusCode::NOT_FOUND,
        r#"{"message":"Branch not found"}"#,
    );
    assert!(github.on_branch("o/r", "main", "a1").await.is_err());
    // main comes back as it was: asked whole, nothing left of the read before it was deleted.
    reads.answer(BRANCH, StatusCode::OK, r#"{"commit":{"sha":"c9"}}"#);
    reads.answer(&compare("c9"), StatusCode::OK, r#"{"status":"diverged"}"#);
    assert!(!github.on_branch("o/r", "main", "a1").await.unwrap());
    assert_eq!(
        conditional(&seen, BRANCH).last(),
        Some(&false),
        "the read after the 404 is not conditional"
    );
}

/// The bug it catches: an error, a refusal or a rate limit answered from the last good read of the same path,
/// which would let a job write on a run GitHub no longer shows, or would not say. A failed read is an error.
#[tokio::test]
async fn a_refused_or_failed_trust_read_never_answers_from_the_kept_one() {
    let reads = TrustReads::new();
    let (github, seen) = trust_github(&reads).await;
    for status in [
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_GATEWAY,
    ] {
        reads.answer(RUN, StatusCode::OK, &trusted_run(PERSON));
        assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
        reads.answer(RUN, status, r#"{"message":"no"}"#);
        assert!(
            github.run_trigger("o/r", 1).await.is_err(),
            "{status} is an error, not the kept run"
        );
        // GitHub answers as before: the next read is whole, not a 304 of the read before the failure.
        reads.answer(RUN, StatusCode::OK, &trusted_run(PERSON));
        let before = conditional(&seen, RUN).len();
        assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
        assert_eq!(
            conditional(&seen, RUN)[before..],
            [false],
            "after {status}, nothing is kept"
        );
    }
    assert_eq!(
        tally_of(&github, Class::Trigger)[2],
        2,
        "the 403 and the 429 are refusals"
    );
}

/// The bug it catches: an answer without an `ETag` leaving the one kept before it, whose `ETag` a later request
/// would then send, and whose body a 304 would then serve, though GitHub answered something else since.
#[tokio::test]
async fn a_trust_answer_without_an_etag_replaces_the_kept_one() {
    let reads = TrustReads::new();
    let (github, seen) = trust_github(&reads).await;
    assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
    reads.answer(
        RUN,
        StatusCode::OK,
        &format!("no-etag:{}", trusted_run(BOT)),
    );
    assert!(!github.run_trigger("o/r", 1).await.unwrap().by_user);
    reads.answer(RUN, StatusCode::OK, &trusted_run(PERSON));
    assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
    assert_eq!(conditional(&seen, RUN), [false, true, false]);
    assert!(
        lock(&github.etags).entries.contains_key(RUN),
        "kept again once it has an ETag"
    );
}

/// The bugs it catches: trust answers kept past the cache's limit, and an answer kept for one run served for
/// another once the cache made room.
#[tokio::test]
async fn trust_reads_under_cache_pressure_stay_within_the_limit_and_answer_their_own_run() {
    let reads = TrustReads::new();
    reads.answer(
        "/repos/o/r/actions/runs/2",
        StatusCode::OK,
        &trusted_run(BOT),
    );
    let (mut github, seen) = trust_github(&reads).await;
    // Room for one run's answer, and every answer may go at once.
    let limit = RUN.len() + trusted_run(BOT).len() + 8;
    *github.etags.get_mut().unwrap() = Conditional::new(limit, Duration::ZERO);
    for _ in 0..3 {
        assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
        assert!(!github.run_trigger("o/r", 2).await.unwrap().by_user);
        assert!(lock(&github.etags).bytes <= limit, "within {limit} bytes");
    }
    assert_eq!(
        conditional(&seen, RUN),
        [false, false, false],
        "run 1's answer made room for run 2's each time"
    );
}

/// The bug it catches: an endpoint counted under another, or under `other` with nothing to tell them apart, so a
/// line cannot say what spends the rate limit; a run read for its trigger counted as any run read.
#[tokio::test]
async fn every_endpoint_is_counted_in_a_class_of_its_own() {
    use super::requests::Class as C;
    let get = |path: &str| C::of(&Method::GET, path);
    let post = |path: &str| C::of(&Method::POST, path);
    let classes = [
        (
            get("/repos/o/r/actions/runs?status=queued&per_page=100"),
            C::Runs,
        ),
        (
            get("/repos/o/r/actions/runs/1/jobs?filter=latest&per_page=100"),
            C::Jobs,
        ),
        (get("/repos/o/r/actions/runners?per_page=100"), C::Runners),
        (get("/repos/o/r/actions/jobs/5"), C::JobRead),
        (
            post("/repos/o/r/actions/runners/generate-jitconfig"),
            C::Registration,
        ),
        (
            C::of(&Method::DELETE, "/repos/o/r/actions/runners/1"),
            C::Delete,
        ),
        (get("/repos/o/r/actions/runs/1"), C::Run),
        (get("/repos/o/r/pulls/7"), C::PullRequest),
        (get("/repos/o/r/branches/release/1%231"), C::Branch),
        (get("/repos/o/r/compare/a2...a1?per_page=1"), C::Compare),
        (get("/repos/o/r"), C::Repository),
        (
            get("/repos/o/r/actions/workflows?per_page=100"),
            C::Workflows,
        ),
        (get("/repos/o/r/git/ref/heads/main"), C::ArchiveRef),
        (get("/repos/o/r/git/trees/a1?recursive=1"), C::ArchiveTree),
        (get("/repos/o/r/git/blobs/b1"), C::ArchiveBlob),
        (
            get("/repos/o/r/actions/runs/1/attempts/2/jobs?per_page=100&page=2"),
            C::AttemptJobs,
        ),
        (post("/repos/o/r/actions/runs/1/cancel"), C::Cancel),
        (post("/repos/o/r/actions/jobs/5/rerun"), C::Rerun),
        (
            post("/repos/o/r/actions/runs/1/rerun-failed-jobs"),
            C::Rerun,
        ),
        (get("/user"), C::Other),
    ];
    for (got, want) in classes {
        assert_eq!(got, want);
    }

    // Through the client: the trust read of run 1 and a job action's read of it are told apart.
    let reads = TrustReads::new();
    reads.answer(
        "/repos/o/r/actions/runs/1/attempts/1/jobs?per_page=100",
        StatusCode::OK,
        r#"{"total_count":0,"jobs":[]}"#,
    );
    reads.answer(
        "/repos/o/r/actions/runs/1/cancel",
        StatusCode::ACCEPTED,
        "{}",
    );
    let (github, _) = trust_github(&reads).await;
    github.run_trigger("o/r", 1).await.unwrap();
    github.action_run("o/r", 1).await.unwrap();
    github.attempt_jobs("o/r", 1, 1).await.unwrap();
    github.cancel_run("o/r", 1).await.unwrap();
    assert_eq!(
        tallies(&github),
        [
            (Class::Trigger, [1, 0, 0]),
            // The job action's read is conditional on what the trust read kept.
            (Class::Run, [0, 1, 0]),
            (Class::AttemptJobs, [1, 0, 0]),
            (Class::Cancel, [1, 0, 0]),
        ]
    );
}

/// The bug it catches: a read nothing asks again (a commit's tree, a blob, the pages of an attempt's jobs) making room
/// in the conditional cache as it is answered, so a large one evicts the trust answers kept for the next job and
/// every later read of them pays the rate limit, though its own answer is dropped right after.
#[tokio::test]
async fn one_off_reads_never_evict_a_kept_trust_answer() {
    use crate::archives::Source as _;
    let reads = TrustReads::new();
    let (mut github, seen) = trust_github(&reads).await;
    // Room for run 1's answer and a little more; every answer may go at once.
    let limit = RUN.len() + trusted_run(PERSON).len() + 200;
    *github.etags.get_mut().unwrap() = Conditional::new(limit, Duration::ZERO);
    assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);

    // Each one-off answer, with an ETag, needs most of the cache's room.
    let pad = "p".repeat(limit - 400);
    reads.answer(
        "/repos/o/r/git/trees/a1?recursive=1",
        StatusCode::OK,
        &format!(r#"{{"truncated":false,"pad":"{pad}","tree":[{{"path":"a.yml","type":"blob","sha":"b1","size":3}}]}}"#),
    );
    reads.answer(
        "/repos/o/r/git/blobs/b1",
        StatusCode::OK,
        &format!(r#"{{"size":3,"encoding":"base64","content":"YWJj","pad":"{pad}"}}"#),
    );
    let attempt = "/repos/o/r/actions/runs/1/attempts/1/jobs?per_page=100";
    let job = |id: u64| {
        format!(
            r#"{{"total_count":2,"pad":"{pad}","jobs":[{{"id":{id},"name":"j{id}","status":"completed","conclusion":"failure"}}]}}"#
        )
    };
    reads.answer(attempt, StatusCode::OK, &format!("next:{}", job(1)));
    reads.answer(&format!("{attempt}&page=2"), StatusCode::OK, &job(2));
    let tree = github.tree("o/r", "a1").await.unwrap();
    assert_eq!(tree.files.len(), 1);
    assert_eq!(github.blob("o/r", "b1", 10).await.unwrap(), b"abc");
    assert_eq!(
        github.attempt_jobs("o/r", 1, 1).await.unwrap().len(),
        2,
        "both pages of the attempt, whole"
    );

    assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
    assert_eq!(
        conditional(&seen, RUN),
        [false, true],
        "run 1 is still asked conditionally"
    );
    assert_eq!(tally_of(&github, Class::Trigger), [1, 1, 0]);
    for (class, read) in [
        (Class::ArchiveTree, [1, 0, 0]),
        (Class::ArchiveBlob, [1, 0, 0]),
        (Class::AttemptJobs, [2, 0, 0]),
    ] {
        assert_eq!(tally_of(&github, class), read, "{class:?} counted as sent");
    }
    let kept: Vec<String> = lock(&github.etags).entries.keys().cloned().collect();
    assert_eq!(kept, [RUN], "no one-off answer is kept");
    // Asked again, a one-off read is whole: never conditional.
    github.tree("o/r", "a1").await.unwrap();
    assert_eq!(
        conditional(&seen, "/repos/o/r/git/trees/a1?recursive=1"),
        [false, false]
    );
}

/// A run read again after its answer was kept, while GitHub cannot be reached or its answer breaks off: the bug it
/// catches is the kept answer served in place of the one that failed, which would let a job write on what GitHub
/// last said rather than what it says now.
#[tokio::test]
async fn a_trust_read_that_fails_in_transport_is_an_error_not_the_kept_answer() {
    let broken = Arc::new(AtomicBool::new(false));
    let reads = TrustReads::new();
    let (answering, breaking) = (reads.clone(), broken.clone());
    let (url, seen) = fake_github(move |request| {
        if breaking.load(Ordering::SeqCst) {
            // The headers come, then the body fails.
            let body = axum::body::Body::from_stream(futures_util::stream::iter([
                Ok::<_, std::io::Error>("{\"event\":"),
                Err(std::io::Error::other("connection reset")),
            ]));
            return ([("etag", "\"other\"")], body).into_response();
        }
        answering.respond(request)
    })
    .await;
    let mut github = client(&url);
    assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
    // A bot re-runs it, and the answer breaks off.
    reads.answer(RUN, StatusCode::OK, &trusted_run(BOT));
    broken.store(true, Ordering::SeqCst);
    assert!(
        matches!(github.run_trigger("o/r", 1).await, Err(Error::Http { .. })),
        "a broken answer is an error"
    );
    broken.store(false, Ordering::SeqCst);
    // GitHub cannot be reached at all.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let reachable = std::mem::replace(&mut github.base, closed);
    assert!(
        matches!(github.run_trigger("o/r", 1).await, Err(Error::Http { .. })),
        "an unreachable GitHub is an error"
    );
    github.base = reachable;
    assert!(
        !github.run_trigger("o/r", 1).await.unwrap().by_user,
        "once GitHub answers, its answer counts: the bot's re-run"
    );
    assert_eq!(conditional(&seen, RUN), [false, true, true]);
}

/// The bug it catches: a refusal for a used-up rate limit, which the client backs off from, answered from the run
/// kept before it, or a read held back by the backoff answered from it without asking GitHub.
#[tokio::test]
async fn a_trust_read_refused_for_the_rate_limit_is_an_error_and_backs_off() {
    for (status, headers) in [
        (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "4000000000"),
            ],
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "600"), ("x-ratelimit-remaining", "12")],
        ),
    ] {
        let limited = Arc::new(AtomicBool::new(false));
        let reads = TrustReads::new();
        let (answering, limiting) = (reads.clone(), limited.clone());
        let (url, seen) = fake_github(move |request| {
            if limiting.load(Ordering::SeqCst) {
                return (status, headers, json(r#"{"message":"rate limit"}"#)).into_response();
            }
            answering.respond(request)
        })
        .await;
        let github = client(&url);
        assert!(github.run_trigger("o/r", 1).await.unwrap().by_user);
        limited.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                github.run_trigger("o/r", 1).await,
                Err(Error::RateLimited { .. })
            ),
            "{status} with its backoff is a rate limit"
        );
        let sent = seen.lock().unwrap().len();
        limited.store(false, Ordering::SeqCst);
        assert!(
            matches!(
                github.run_trigger("o/r", 1).await,
                Err(Error::RateLimited { .. })
            ),
            "held back until the limit resets, not answered from the kept run"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            sent,
            "nothing sent while backing off"
        );
        assert!(github.blocked_until().is_some());
    }
}

/// One job as GitHub's jobs API gives it, in a run's jobs page and from `GET /actions/jobs/{id}` alike: run `run`'s
/// attempt 2, under the run's name, with `fields` in place of its status, conclusion, end and runner.
fn listed_job(id: u64, run: u64, fields: &str) -> String {
    format!(
        r#"{{"id":{id},"run_id":{run},"run_attempt":2,"name":"test {id}","workflow_name":"deploy {run}",
        "head_branch":"main","html_url":"https://github.com/o/r/actions/runs/{run}/job/{id}",
        "labels":["self-hosted","app"],"created_at":"2026-11-14T10:00:00Z","started_at":"2026-11-14T10:00:10Z",
        "steps":[{{"number":1,"name":"Set up job","status":"completed","conclusion":"success",
        "started_at":"2026-11-14T10:00:10Z","completed_at":"2026-11-14T10:00:12Z"}}],{fields}}}"#
    )
}

const RUNNING: &str =
    r#""status":"in_progress","conclusion":null,"completed_at":null,"runner_name":"runnerd-a""#;
const PASSED: &str = r#""status":"completed","conclusion":"success","completed_at":"2026-11-14T10:01:00Z","runner_name":"runnerd-b""#;

/// Run 20, queued, with queued job 3 and running job 2; run 30, in progress, with no queued job and completed job 4;
/// both of workflow 7, `CI`. Each job read answers what its page shows.
async fn pages_github() -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => {
            json(r#"{"workflow_runs":[{"id":20,"workflow_id":7}]}"#)
        }
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            json(r#"{"workflow_runs":[{"id":30,"workflow_id":7}]}"#)
        }
        "/repos/o/r/actions/runs/20/jobs?filter=latest&per_page=100" => json(&format!(
            r#"{{"jobs":[{},{}]}}"#,
            listed_job(2, 20, RUNNING),
            listed_job(
                3,
                20,
                r#""status":"queued","conclusion":null,"completed_at":null,"runner_name":null"#
            )
        )),
        "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100" => {
            json(&format!(r#"{{"jobs":[{}]}}"#, listed_job(4, 30, PASSED)))
        }
        "/repos/o/r/actions/jobs/2" => json(&listed_job(2, 20, RUNNING)),
        "/repos/o/r/actions/jobs/4" => json(&listed_job(4, 30, PASSED)),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await
}

/// The bugs it catches: a listed job decoded with fewer fields than a job read gives (steps, runner, attempt, times,
/// URL, labels), one that keeps the run's name for its workflow's, or one that costs a request more than the listing.
#[tokio::test]
async fn a_listing_gives_the_followed_jobs_its_pages_show_as_a_job_read_describes_them() {
    let (url, seen) = pages_github().await;
    let github = client(&url);
    let plain = github
        .queued_jobs("o/r", &std::collections::BTreeSet::new())
        .await
        .unwrap();
    assert!(plain.observed.is_empty(), "no job followed, none given");
    let asked = uris(&seen).len();
    let listing = github
        .queued_jobs("o/r", &[2, 3, 4, 9].into_iter().collect())
        .await
        .unwrap();
    assert_eq!(
        uris(&seen).len() - asked,
        4,
        "the same four pages as a listing that follows nothing: {:?}",
        &uris(&seen)[asked..]
    );
    assert_eq!(
        listing.queued.iter().map(|job| job.id).collect::<Vec<_>>(),
        [3],
        "a queued job is the listing's, not an observation"
    );
    assert_eq!(listing.queued, plain.queued);
    let read = [
        github.job("o/r", 2).await.unwrap(),
        github.job("o/r", 4).await.unwrap(),
    ];
    assert_eq!(listing.observed, read, "each as its read describes it");
    assert_eq!(read[0].workflow_name.as_deref(), Some("CI"));
    assert_eq!(read[1].run_attempt, 2);
    assert_eq!(read[0].steps.len(), 1);
}

/// The bugs it catches: a listed job given though its details lack what a read would say, or though naming its
/// workflow would take a read, or a listing that fails for a job its own decoding cannot use.
#[tokio::test]
async fn a_listing_leaves_out_a_followed_job_its_page_cannot_answer_for_whole() {
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => json(r#"{"workflow_runs":[]}"#),
        // Run 50's workflow is not known: naming it would take a read of the run.
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => json(
            r#"{"workflow_runs":[{"id":30,"workflow_id":7},{"id":50}]}"#,
        ),
        "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100" => json(&format!(
            r#"{{"jobs":[{},{},{},{},{}]}}"#,
            // Queued: the listing names run 30's workflow for it.
            listed_job(6, 30, r#""status":"queued","conclusion":null,"completed_at":null,"runner_name":null"#),
            // In progress, but GitHub names no runner yet.
            listed_job(1, 30, r#""status":"in_progress","conclusion":null,"completed_at":null,"runner_name":null"#),
            // Completed without a conclusion.
            listed_job(2, 30, r#""status":"completed","conclusion":null,"completed_at":null,"runner_name":"runnerd-a""#),
            // A step that does not parse as a job read's.
            r#"{"id":3,"run_id":30,"name":"odd","status":"completed","conclusion":"success","labels":[],
               "completed_at":"2026-11-14T10:01:00Z","steps":[{"number":"one"}]}"#,
            // Not followed.
            listed_job(4, 30, PASSED),
        )),
        "/repos/o/r/actions/runs/50/jobs?filter=latest&per_page=100" => {
            json(&format!(r#"{{"jobs":[{}]}}"#, listed_job(5, 50, PASSED)))
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let listing = client(&url)
        .queued_jobs("o/r", &[1, 2, 3, 5].into_iter().collect())
        .await
        .unwrap();
    assert_eq!(
        listing.queued.iter().map(|job| job.id).collect::<Vec<_>>(),
        [6]
    );
    assert!(
        listing.observed.is_empty(),
        "nothing to stand for a read: {:?}",
        listing.observed
    );
    assert_eq!(
        listing.queued[0].workflow.as_deref(),
        Some("CI"),
        "run 30's workflow is known"
    );
    assert!(
        !uris(&seen)
            .iter()
            .any(|uri| uri == "/repos/o/r/actions/runs/50"),
        "no read of run 50 for its workflow: {:?}",
        uris(&seen)
    );
}

/// `listed_job(id, run, fields)` without the keys `drop`, and with `set` written over it.
fn listed_job_with(
    id: u64,
    run: u64,
    fields: &str,
    drop: &[&str],
    set: &[(&str, serde_json::Value)],
) -> String {
    let mut job: serde_json::Value = serde_json::from_str(&listed_job(id, run, fields)).unwrap();
    let object = job.as_object_mut().unwrap();
    for key in drop {
        object.remove(*key);
    }
    for (key, value) in set {
        object.insert((*key).to_owned(), value.clone());
    }
    job.to_string()
}

/// A listing of run 30 alone, its workflow known through its queued job 99, whose page shows `jobs` besides.
async fn one_run_github(jobs: Vec<String>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let queued = listed_job(
        99,
        30,
        r#""status":"queued","conclusion":null,"completed_at":null,"runner_name":null"#,
    );
    let page = format!(r#"{{"jobs":[{queued},{}]}}"#, jobs.join(","));
    fake_github(move |request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => json(r#"{"workflow_runs":[]}"#),
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            json(r#"{"workflow_runs":[{"id":30,"workflow_id":7}]}"#)
        }
        "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100" => json(&page),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await
}

/// The bugs it catches: a listed job missing a field a job read has (its attempt, its steps, or a nullable field left
/// out; one without labels fails the listing itself, as before) taken with `JobDetails`' defaults, which would record attempt 1, no steps or no pool for good once it
/// completed; a time that does not parse; and a completed job shown started with no runner, a partial answer, taken
/// as whole, nor one shown with a runner and no start, while one that never started and had no runner, which GitHub
/// gives for a job cancelled in the queue, is.
#[tokio::test]
async fn a_listed_job_missing_what_a_read_gives_is_left_for_the_read() {
    use serde_json::Value;
    let cancelled = r#""status":"completed","conclusion":"cancelled","completed_at":"2026-11-14T10:01:00Z","runner_name":null"#;
    let jobs = vec![
        listed_job_with(1, 30, PASSED, &["run_attempt"], &[]),
        listed_job_with(2, 30, PASSED, &["steps"], &[]),
        listed_job_with(4, 30, PASSED, &["head_branch"], &[]),
        listed_job_with(5, 30, PASSED, &[], &[("completed_at", Value::from("soon"))]),
        listed_job_with(6, 30, cancelled, &[], &[]),
        listed_job_with(7, 30, cancelled, &[], &[("started_at", Value::Null)]),
        listed_job_with(
            8,
            30,
            PASSED,
            &[],
            &[(
                "steps",
                serde_json::json!([{"number":1,"name":"Run","status":"in_progress",
                "conclusion":null,"started_at":"2026-11-14T10:00:10Z","completed_at":null}]),
            )],
        ),
        listed_job_with(9, 30, PASSED, &[], &[]),
        // Completed on a runner, without a start: a runner's job always has one.
        listed_job_with(10, 30, PASSED, &[], &[("started_at", Value::Null)]),
    ];
    let (url, _) = one_run_github(jobs).await;
    let listing = client(&url)
        .queued_jobs("o/r", &(1..=10).collect())
        .await
        .unwrap();
    assert_eq!(
        listing
            .observed
            .iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        [7, 9],
        "only the whole ones: job 7 cancelled before it started, job 9 complete"
    );
    assert_eq!(listing.observed[0].runner_name, None);
    assert_eq!(listing.queued.len(), 1, "the listing itself answers");
}

/// The bug it catches: a job GitHub moved between pages while they were read, shown in progress on one and completed
/// on the next, answered by whichever page sorts first, so a running read could stand on what the listing already saw
/// is over. Shown twice alike, it is one answer.
#[tokio::test]
async fn a_job_two_pages_show_otherwise_is_left_for_the_read() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => json(r#"{"workflow_runs":[]}"#),
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            json(r#"{"workflow_runs":[{"id":30,"workflow_id":7}]}"#)
        }
        "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100" => page(
            request,
            &format!(
                r#"{{"jobs":[{},{},{}]}}"#,
                listed_job(
                    99,
                    30,
                    r#""status":"queued","conclusion":null,"completed_at":null,"runner_name":null"#
                ),
                listed_job(1, 30, RUNNING),
                listed_job(2, 30, PASSED)
            ),
            true,
        ),
        "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100&page=2" => page(
            request,
            &format!(
                r#"{{"jobs":[{},{}]}}"#,
                listed_job(1, 30, PASSED),
                listed_job(2, 30, PASSED)
            ),
            false,
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let listing = client(&url)
        .queued_jobs("o/r", &[1, 2].into_iter().collect())
        .await
        .unwrap();
    assert_eq!(
        listing
            .observed
            .iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        [2],
        "job 1 showed in progress and completed; job 2 alike twice"
    );
}

/// Run 30's jobs as GitHub serves them in `phase`: job 2 running in 0, completed in 1, with queued job 99 beside it.
/// Each answer's `ETag` is its path and phase, so an unchanged phase is a 304.
async fn lifecycle_github(phase: Arc<AtomicUsize>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_github(move |request| {
        let now = phase.load(Ordering::SeqCst);
        let job = if now == 0 { RUNNING } else { PASSED };
        let body = match request.uri.as_str() {
            "/repos/o/r/actions/runs?status=queued&per_page=100" => {
                r#"{"workflow_runs":[]}"#.to_owned()
            }
            "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
                r#"{"workflow_runs":[{"id":30,"workflow_id":7}]}"#.to_owned()
            }
            "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100" => format!(
                r#"{{"jobs":[{},{}]}}"#,
                listed_job(
                    99,
                    30,
                    r#""status":"queued","conclusion":null,"completed_at":null,"runner_name":null"#
                ),
                listed_job(2, 30, job)
            ),
            "/repos/o/r/actions/jobs/2" => listed_job(2, 30, job),
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        let etag = format!("\"{}#{now}\"", request.uri);
        if request
            .headers
            .get("if-none-match")
            .is_some_and(|sent| sent.as_bytes() == etag.as_bytes())
        {
            return StatusCode::NOT_MODIFIED.into_response();
        }
        ([("etag", etag)], json(&body)).into_response()
    })
    .await
}

/// What a lifecycle of job 2 cost the client, by class in `Class::ALL`'s order: fresh answers and all requests.
fn spent(github: &Github) -> Vec<(Class, (u64, u64))> {
    let report = github.requests();
    Class::ALL
        .iter()
        .map(|class| (*class, report.tally(*class)))
        .filter(|(_, tally)| tally.total() > 0)
        .map(|(class, tally)| (class, (tally.fresh, tally.total())))
        .collect()
}

/// The bugs it catches: a saving that only moves requests to the jobs pages, or one counted in logical calls while
/// GitHub's 304s already made the reads free. Three ticks of job 2 (running, the same again, completed), each listing
/// its run and then describing it: once by a job read after a listing that follows nothing, once from the listing.
#[tokio::test]
async fn a_lifecycle_answered_by_its_listing_spends_no_job_read_fresh_or_unchanged() {
    let mut spends = Vec::new();
    let mut answers = Vec::new();
    for answered_by_listing in [false, true] {
        let phase = Arc::new(AtomicUsize::new(0));
        let (url, _) = lifecycle_github(phase.clone()).await;
        let github = client(&url);
        let following: std::collections::BTreeSet<u64> = if answered_by_listing {
            [2].into_iter().collect()
        } else {
            std::collections::BTreeSet::new()
        };
        let mut described = Vec::new();
        for tick in 0..3 {
            phase.store(usize::from(tick == 2), Ordering::SeqCst);
            let listing = github.queued_jobs("o/r", &following).await.unwrap();
            let details = match listing.observed.into_iter().find(|job| job.id == 2) {
                Some(details) => details,
                None => github.job("o/r", 2).await.unwrap(),
            };
            described.push(details);
        }
        spends.push(spent(&github));
        answers.push(described);
    }
    assert_eq!(answers[0], answers[1], "the same answers, tick by tick");
    assert_eq!(answers[1][2].status, "completed");
    let (read, listed) = (&spends[0], &spends[1]);
    let of = |spend: &[(Class, (u64, u64))], class| {
        spend
            .iter()
            .find(|(each, _)| *each == class)
            .map(|(_, spent)| *spent)
    };
    assert_eq!(
        of(read, Class::JobRead),
        Some((2, 3)),
        "read: two fresh answers and one 304: {read:?}"
    );
    assert_eq!(of(listed, Class::JobRead), None, "no job read: {listed:?}");
    assert!(
        of(listed, Class::Jobs).is_some_and(|(_, total)| total == 3),
        "the run's jobs page once a tick: {listed:?}"
    );
    let others: Vec<_> = read
        .iter()
        .filter(|(class, _)| *class != Class::JobRead)
        .copied()
        .collect();
    assert_eq!(
        &others, listed,
        "every other class spent alike, the jobs pages too"
    );
}

/// The bug it catches: a listed job without labels taken with no labels, which would place it in no pool. The listing
/// needs them for its own queued jobs, so such a page fails the listing whole, as before observations.
#[tokio::test]
async fn a_listed_job_without_labels_fails_the_listing() {
    let (url, _) = one_run_github(vec![listed_job_with(1, 30, PASSED, &["labels"], &[])]).await;
    let listing = client(&url)
        .queued_jobs("o/r", &[1].into_iter().collect())
        .await;
    assert!(
        matches!(&listing, Err(Error::Decode { source, .. }) if source.to_string().contains("labels")),
        "the page does not parse: {listing:?}"
    );
}

/// The bugs it catches: a job one page shows whole and another in part, or on another runner, answered by the whole
/// one, so a running read could stand on details another page of the same listing contradicts. A job shown twice
/// alike is still one answer.
#[tokio::test]
async fn a_job_another_page_shows_in_part_or_on_another_runner_is_left_for_the_read() {
    let other_runner =
        r#""status":"in_progress","conclusion":null,"completed_at":null,"runner_name":"runnerd-x""#;
    let first = [
        listed_job(
            99,
            30,
            r#""status":"queued","conclusion":null,"completed_at":null,"runner_name":null"#,
        ),
        listed_job(1, 30, RUNNING),
        listed_job_with(2, 30, PASSED, &["steps"], &[]),
        listed_job(3, 30, RUNNING),
        listed_job(4, 30, PASSED),
    ]
    .join(",");
    let second = [
        // Completed, its steps left out: the whole running answer above is not taken either.
        listed_job_with(1, 30, PASSED, &["steps"], &[]),
        // The whole answer after the part.
        listed_job(2, 30, PASSED),
        listed_job(3, 30, other_runner),
        listed_job(4, 30, PASSED),
    ]
    .join(",");
    let (url, _) = fake_github(move |request| match request.uri.as_str() {
        "/repos/o/r/actions/runs?status=queued&per_page=100" => json(r#"{"workflow_runs":[]}"#),
        "/repos/o/r/actions/runs?status=in_progress&per_page=100" => {
            json(r#"{"workflow_runs":[{"id":30,"workflow_id":7}]}"#)
        }
        "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100" => {
            page(request, &format!(r#"{{"jobs":[{first}]}}"#), true)
        }
        "/repos/o/r/actions/runs/30/jobs?filter=latest&per_page=100&page=2" => {
            page(request, &format!(r#"{{"jobs":[{second}]}}"#), false)
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let listing = client(&url)
        .queued_jobs("o/r", &(1..=4).collect())
        .await
        .unwrap();
    assert_eq!(
        listing
            .observed
            .iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        [4],
        "jobs 1 and 2 were shown in part, job 3 on two runners; job 4 alike twice"
    );
}

// The token lifecycle (ADR 0181): an expiring token kept until shortly before it expires, read once for everyone
// waiting, never served past either clock's end, and a failing source waited out.

use super::token::{self, BACKOFF_FIRST, MINT_DEADLINE, Now, REFRESH_MARGIN, Tokens};
use super::{Credentials, ExpiringSource, ExpiringToken, PublicReads};

/// `from` less `less`.
fn before(from: Duration, less: Duration) -> Duration {
    from.checked_sub(less).unwrap()
}

/// A clock a test moves: both its sides, or one.
#[derive(Clone)]
struct FakeClock(Arc<Mutex<Now>>);

impl FakeClock {
    fn new() -> FakeClock {
        FakeClock(Arc::new(Mutex::new(Now {
            monotonic: std::time::Instant::now(),
            wall: SystemTime::now(),
        })))
    }

    fn now(&self) -> Now {
        *self.0.lock().unwrap()
    }

    fn clock(&self) -> token::Clock {
        let clock = self.clone();
        Arc::new(move || clock.now())
    }

    /// Time passes on both clocks, as it does while the host is awake.
    fn pass(&self, by: Duration) {
        let mut now = self.0.lock().unwrap();
        now.monotonic += by;
        now.wall += by;
    }

    /// The wall clock alone moves: the host slept, or the clock was set forward.
    fn jump_wall(&self, by: Duration) {
        self.0.lock().unwrap().wall += by;
    }

    /// The monotonic clock alone moves: the wall clock was set back as time passed.
    fn pass_monotonic(&self, by: Duration) {
        self.0.lock().unwrap().monotonic += by;
    }
}

/// What a fake minting source answers with each read, by its number from 1.
type Answer = dyn Fn(usize, SystemTime) -> Result<ExpiringToken, Error> + Send + Sync;

/// A minting source: counts its reads in `mints`, waits for a permit of `gate` when one is given, and answers with
/// `answer`, the read's number and the clock's wall time.
fn minting(
    clock: &FakeClock,
    mints: &Arc<AtomicUsize>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    answer: impl Fn(usize, SystemTime) -> Result<ExpiringToken, Error> + Send + Sync + 'static,
) -> ExpiringSource {
    let (clock, mints) = (clock.clone(), mints.clone());
    let answer: Arc<Answer> = Arc::new(answer);
    Arc::new(move || {
        let (clock, mints, gate, answer) =
            (clock.clone(), mints.clone(), gate.clone(), answer.clone());
        Box::pin(async move {
            let read = mints.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            answer(read, clock.now().wall)
        })
    })
}

/// `t{read}`, expiring an hour after `wall`.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the shape of a minting source's answer"
)]
fn hourly(read: usize, wall: SystemTime) -> Result<ExpiringToken, Error> {
    Ok(ExpiringToken {
        token: Token(format!("t{read}")),
        expires_at: wall + Duration::from_hours(1),
    })
}

fn expiring_tokens(clock: &FakeClock, source: ExpiringSource) -> Arc<Tokens> {
    let mut tokens = Tokens::new(token::Source::Expiring(source));
    tokens.set_clock(clock.clock());
    Arc::new(tokens)
}

async fn value(tokens: &Tokens, refused: Option<&str>) -> Result<String, Error> {
    let refused = refused.map(|refused| Token(refused.to_owned()));
    tokens
        .get(refused.as_ref())
        .await
        .map(|granted| granted.token().0.clone())
}

#[tokio::test]
async fn an_expiring_token_is_kept_until_its_margin_and_then_read_again() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let tokens = expiring_tokens(&clock, minting(&clock, &mints, None, hourly));
    assert_eq!(value(&tokens, None).await.unwrap(), "t1");
    clock.pass(before(
        before(Duration::from_hours(1), REFRESH_MARGIN),
        Duration::from_secs(1),
    ));
    assert_eq!(
        value(&tokens, None).await.unwrap(),
        "t1",
        "still outside the margin"
    );
    clock.pass(Duration::from_secs(1));
    assert_eq!(
        value(&tokens, None).await.unwrap(),
        "t2",
        "read again inside the margin, before it expires"
    );
    assert_eq!(mints.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_wall_clock_jump_or_a_sleep_past_the_expiry_reads_again_and_so_does_a_clock_set_back() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let tokens = expiring_tokens(&clock, minting(&clock, &mints, None, hourly));
    assert_eq!(value(&tokens, None).await.unwrap(), "t1");
    // The host slept two hours: the monotonic clock did not move.
    clock.jump_wall(Duration::from_hours(2));
    assert_eq!(value(&tokens, None).await.unwrap(), "t2", "never t1 again");
    // The wall clock was set back as two hours passed: only the monotonic clock says so.
    clock.pass_monotonic(Duration::from_hours(2));
    assert_eq!(value(&tokens, None).await.unwrap(), "t3");
    // An expiry past the longest a token lasts is held to it.
    let far = Arc::new(AtomicUsize::new(0));
    let tokens = expiring_tokens(
        &clock,
        minting(&clock, &far, None, |read, wall| {
            Ok(ExpiringToken {
                token: Token(format!("f{read}")),
                expires_at: wall + Duration::from_hours(24),
            })
        }),
    );
    assert_eq!(value(&tokens, None).await.unwrap(), "f1");
    clock.pass(before(token::MAX_LIFETIME, REFRESH_MARGIN));
    assert_eq!(value(&tokens, None).await.unwrap(), "f2");
}

#[tokio::test]
async fn a_token_that_is_invalid_or_expires_within_the_margin_is_refused_and_its_source_waited_out()
{
    let clock = FakeClock::new();
    for (value_of, expires_in) in [
        ("", Duration::from_hours(1)),
        ("has space", Duration::from_hours(1)),
        ("ghs_soon_SENTINEL", REFRESH_MARGIN),
        ("ghs_past_SENTINEL", Duration::ZERO),
    ] {
        let mints = Arc::new(AtomicUsize::new(0));
        let tokens = expiring_tokens(
            &clock,
            minting(&clock, &mints, None, move |_, wall| {
                Ok(ExpiringToken {
                    token: Token(value_of.to_owned()),
                    expires_at: wall + expires_in,
                })
            }),
        );
        let refused = value(&tokens, None).await.unwrap_err();
        assert!(matches!(refused, Error::Token(_)), "{refused}");
        let text = format!("{refused} {refused:?}");
        assert!(
            value_of.is_empty() || !text.contains(value_of),
            "the refusal does not show the token: {text}"
        );
        // Asked again within the backoff: no read.
        for _ in 0..5 {
            assert!(value(&tokens, None).await.is_err());
        }
        assert_eq!(mints.load(Ordering::SeqCst), 1, "one read, not one a poll");
    }
    // An expiry already past is refused too.
    let mints = Arc::new(AtomicUsize::new(0));
    let tokens = expiring_tokens(
        &clock,
        minting(&clock, &mints, None, |_, wall| {
            Ok(ExpiringToken {
                token: Token("ghs_old".to_owned()),
                expires_at: wall - Duration::from_secs(60),
            })
        }),
    );
    assert!(value(&tokens, None).await.is_err());
}

#[tokio::test]
async fn a_failing_source_is_tried_again_only_after_a_backoff_that_grows_and_is_reset_by_a_token() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let failing = Arc::new(AtomicBool::new(true));
    let fails = failing.clone();
    let tokens = expiring_tokens(
        &clock,
        minting(&clock, &mints, None, move |read, wall| {
            if fails.load(Ordering::SeqCst) {
                Err(Error::Token("GitHub refused the mint".to_owned()))
            } else {
                hourly(read, wall)
            }
        }),
    );
    let reads = || mints.load(Ordering::SeqCst);
    assert!(value(&tokens, None).await.is_err());
    assert_eq!(reads(), 1);
    clock.pass(before(BACKOFF_FIRST, Duration::from_secs(1)));
    assert!(value(&tokens, None).await.is_err());
    assert_eq!(reads(), 1, "within the first backoff");
    clock.pass(Duration::from_secs(1));
    assert!(value(&tokens, None).await.is_err());
    assert_eq!(reads(), 2, "the first backoff passed");
    clock.pass(BACKOFF_FIRST);
    assert!(value(&tokens, None).await.is_err());
    assert_eq!(reads(), 2, "the second backoff is twice the first");
    clock.pass(BACKOFF_FIRST);
    failing.store(false, Ordering::SeqCst);
    assert_eq!(value(&tokens, None).await.unwrap(), "t3");
    // Expiring now, and the source fails again: the backoff starts over at the first.
    clock.pass(Duration::from_hours(1));
    failing.store(true, Ordering::SeqCst);
    assert!(value(&tokens, None).await.is_err());
    clock.pass(BACKOFF_FIRST);
    failing.store(false, Ordering::SeqCst);
    assert_eq!(value(&tokens, None).await.unwrap(), "t5");
    // A backoff is bounded.
    assert_eq!(token::backoff_of(40), token::BACKOFF_MOST);
}

#[tokio::test]
async fn cold_expiring_and_refused_requests_at_once_read_the_token_once() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tokens = expiring_tokens(&clock, minting(&clock, &mints, Some(gate.clone()), hourly));
    let spawn = |refused: Option<&'static str>| {
        let tokens = tokens.clone();
        tokio::spawn(async move { value(&tokens, refused).await })
    };
    // Cold.
    let waiting: Vec<_> = (0..5).map(|_| spawn(None)).collect();
    reads_reach(&mints, 1).await;
    gate.add_permits(1);
    for waiter in waiting {
        assert_eq!(waiter.await.unwrap().unwrap(), "t1");
    }
    assert_eq!(mints.load(Ordering::SeqCst), 1);
    // Expiring, and refused at once.
    clock.pass(before(Duration::from_hours(1), REFRESH_MARGIN));
    let mut waiting: Vec<_> = (0..3).map(|_| spawn(None)).collect();
    waiting.extend((0..3).map(|_| spawn(Some("t1"))));
    reads_reach(&mints, 2).await;
    // Give every waiter its turn to join before the read ends.
    tokio::time::sleep(Duration::from_millis(20)).await;
    gate.add_permits(1);
    for waiter in waiting {
        assert_eq!(waiter.await.unwrap().unwrap(), "t2");
    }
    assert_eq!(mints.load(Ordering::SeqCst), 2, "one read for all six");
}

#[tokio::test]
async fn a_late_refusal_of_an_old_token_keeps_the_newer_one() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let tokens = expiring_tokens(&clock, minting(&clock, &mints, None, hourly));
    assert_eq!(value(&tokens, None).await.unwrap(), "t1");
    assert_eq!(value(&tokens, Some("t1")).await.unwrap(), "t2");
    assert_eq!(
        value(&tokens, Some("t1")).await.unwrap(),
        "t2",
        "a request that sent t1 before t2 came is refused late"
    );
    assert_eq!(mints.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_waiter_that_leaves_strands_neither_the_read_nor_the_others() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tokens = expiring_tokens(&clock, minting(&clock, &mints, Some(gate.clone()), hourly));
    let first = {
        let tokens = tokens.clone();
        tokio::spawn(async move { value(&tokens, None).await })
    };
    reads_reach(&mints, 1).await;
    let second = {
        let tokens = tokens.clone();
        tokio::spawn(async move { value(&tokens, None).await })
    };
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    gate.add_permits(1);
    assert_eq!(second.await.unwrap().unwrap(), "t1");
    assert_eq!(
        value(&tokens, None).await.unwrap(),
        "t1",
        "the read kept it"
    );
    assert_eq!(mints.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_read_that_panics_or_times_out_answers_every_waiter_and_waits_out_a_backoff() {
    let clock = FakeClock::new();
    for hangs in [false, true] {
        let mints = Arc::new(AtomicUsize::new(0));
        let counted = mints.clone();
        let source: ExpiringSource = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if hangs {
                    std::future::pending::<()>().await;
                }
                panic!("the source panicked");
            })
        });
        let tokens = expiring_tokens(&clock, source);
        let waiting: Vec<_> = (0..4)
            .map(|_| {
                let tokens = tokens.clone();
                tokio::spawn(async move { value(&tokens, None).await })
            })
            .collect();
        for waiter in waiting {
            let answer = tokio::time::timeout(MINT_DEADLINE * 2, waiter)
                .await
                .expect("every waiter is answered")
                .unwrap();
            assert!(answer.is_err(), "hangs {hangs}: {answer:?}");
        }
        assert!(
            value(&tokens, None).await.is_err(),
            "within the backoff, no read"
        );
        assert_eq!(mints.load(Ordering::SeqCst), 1, "hangs {hangs}");
        clock.pass(BACKOFF_FIRST);
    }
}

/// A read whose runtime stops ends as failed: the next runtime's request is not left waiting on it.
#[test]
fn a_read_dropped_with_its_runtime_ends_and_the_next_runtime_reads_again() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let first_hangs = {
        let counted = mints.clone();
        let clock = clock.clone();
        let source: ExpiringSource = Arc::new(move || {
            let read = counted.fetch_add(1, Ordering::SeqCst) + 1;
            let wall = clock.now().wall;
            Box::pin(async move {
                if read == 1 {
                    std::future::pending::<()>().await;
                }
                hourly(read, wall)
            })
        });
        source
    };
    let tokens = expiring_tokens(&clock, first_hangs);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let waited = tokio::time::timeout(Duration::from_millis(50), value(&tokens, None)).await;
        assert!(waited.is_err(), "still reading");
    });
    let (stopped, shut) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(runtime);
        let _ = stopped.send(());
    });
    assert!(
        shut.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the runtime stops with the read pending"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let answer = tokio::time::timeout(Duration::from_secs(5), value(&tokens, None))
            .await
            .expect("not left waiting on the dropped read");
        assert!(answer.is_err(), "the dropped read failed");
        clock.pass(BACKOFF_FIRST);
        assert_eq!(value(&tokens, None).await.unwrap(), "t2");
    });
}

/// The permit is asked again once a refreshed token is in hand: a reload while it is minted withdraws the action.
#[tokio::test]
async fn a_job_actions_permit_is_asked_again_after_its_token_is_refreshed() {
    let (url, seen) = fake_github(|request| {
        if request
            .headers
            .get("authorization")
            .map(HeaderValue::as_bytes)
            == Some(b"Bearer t3")
        {
            StatusCode::UNAUTHORIZED.into_response()
        } else {
            StatusCode::ACCEPTED.into_response()
        }
    })
    .await;
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let github = Arc::new(
        Github::with_credentials(
            &url,
            Credentials::Expiring(minting(&clock, &mints, Some(gate.clone()), hourly)),
            PublicReads::Authenticated,
        )
        .unwrap()
        .with_token_clock(clock.clock()),
    );
    assert_eq!(
        github
            .job_action("o/r", JobActionRequest::CancelRun { run: 1 }, &go)
            .await,
        Ok(202)
    );
    // The token is about to expire; the permit is withdrawn while the new one is minted.
    clock.pass(before(Duration::from_hours(1), REFRESH_MARGIN));
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
                .job_action("o/r", JobActionRequest::CancelRun { run: 1 }, &permit)
                .await
        })
    };
    reads_reach(&mints, 2).await;
    allowed.store(false, Ordering::SeqCst);
    gate.add_permits(1);
    assert_eq!(sending.await.unwrap(), Err(ActionFailure::Withdrawn));
    assert_eq!(asked.load(Ordering::SeqCst), 1, "asked once t2 was in hand");
    assert_eq!(seen.lock().unwrap().len(), 1, "only the first action went");
    // A 401 for t3 reads once more and sends once more: t4.
    gate.add_permits(2);
    clock.pass(before(Duration::from_hours(1), REFRESH_MARGIN));
    assert_eq!(
        github
            .job_action("o/r", JobActionRequest::CancelRun { run: 1 }, &go)
            .await,
        Ok(202)
    );
    assert_eq!(mints.load(Ordering::SeqCst), 4);
    let bearers: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|seen| seen.headers["authorization"].to_str().unwrap().to_owned())
        .collect();
    assert_eq!(bearers, ["Bearer t1", "Bearer t3", "Bearer t4"]);
}

#[tokio::test]
async fn an_expiring_token_never_shows_in_debug_output_or_errors() {
    const MINTED: &str = "ghs_mintedSentinelValue0123456789";
    let (url, _) = fake_github(|_| StatusCode::UNAUTHORIZED.into_response()).await;
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let source = minting(&clock, &mints, None, |_, wall| {
        Ok(ExpiringToken {
            token: Token(MINTED.to_owned()),
            expires_at: wall + Duration::from_hours(1),
        })
    });
    let minted = source().await.unwrap();
    let credentials = Credentials::Expiring(source);
    let credentials_text = format!("{credentials:?}");
    let github = Github::with_credentials(&url, credentials, PublicReads::Anonymous)
        .unwrap()
        .with_token_clock(clock.clock());
    let refused = github.runners("o/r").await.unwrap_err();
    for text in [
        format!("{github:?}"),
        format!("{minted:?}"),
        credentials_text,
        format!("{refused:?} {refused}"),
    ] {
        assert!(!text.contains(MINTED), "{text}");
    }
}

// A pinned action's repository read anonymously (ADR 0181): no credential, no redirect, and nothing of the
// authenticated reads' rate limit, counts, backoff or kept answers touched.

/// A fake GitHub for the archives' reads: `o/app` is private and answers only the token; `actions/checkout` is public
/// and answers only without one, conditionally on its tag; `o/moved` redirects to `o/new`; `o/private` is not there to
/// anyone without the token; `o/limited` answers that the anonymous limit is used up.
async fn archive_github() -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_github(|request| {
        let bearer = request
            .headers
            .get("authorization")
            .map(HeaderValue::as_bytes);
        let token = format!("Bearer {TOKEN}");
        let mut response = match (request.uri.as_str(), bearer) {
            ("/repos/o/app", Some(sent)) if sent == token.as_bytes() => json(
                r#"{"full_name":"o/app","visibility":"private","default_branch":"main"}"#,
            ),
            ("/repos/actions/checkout", None) => {
                if request.headers.get("if-none-match").map(HeaderValue::as_bytes) == Some(b"\"c1\"")
                {
                    StatusCode::NOT_MODIFIED.into_response()
                } else {
                    let mut response = json(
                        r#"{"full_name":"actions/checkout","visibility":"public","default_branch":"main"}"#,
                    );
                    response
                        .headers_mut()
                        .insert("etag", HeaderValue::from_static("\"c1\""));
                    response
                }
            }
            ("/repos/o/moved", None) => {
                (StatusCode::MOVED_PERMANENTLY, [("location", "/repos/o/new")]).into_response()
            }
            ("/repos/o/new", _) => json(
                r#"{"full_name":"o/new","visibility":"public","default_branch":"main"}"#,
            ),
            ("/repos/o/limited", None) => {
                let mut response = StatusCode::FORBIDDEN.into_response();
                let headers = response.headers_mut();
                headers.insert("x-ratelimit-limit", HeaderValue::from_static("60"));
                headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
                headers.insert("x-ratelimit-reset", HeaderValue::from_static("4000000000"));
                headers.insert("x-ratelimit-resource", HeaderValue::from_static("core"));
                return response;
            }
            (_, Some(_)) if request.uri.starts_with("/repos/actions/") => {
                (StatusCode::INTERNAL_SERVER_ERROR, "a token went to a public read").into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        };
        let headers = response.headers_mut();
        let anonymous = bearer.is_none();
        headers.insert(
            "x-ratelimit-limit",
            HeaderValue::from_static(if anonymous { "60" } else { "5000" }),
        );
        headers.insert(
            "x-ratelimit-remaining",
            HeaderValue::from_static(if anonymous { "1" } else { "4000" }),
        );
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("4000000000"));
        headers.insert("x-ratelimit-resource", HeaderValue::from_static("core"));
        response
    })
    .await
}

fn anonymous_client(url: &str) -> Github {
    Github::with_credentials(
        url,
        Credentials::Command(Box::new(|| Ok(Token(TOKEN.to_owned())))),
        PublicReads::Anonymous,
    )
    .unwrap()
}

#[tokio::test]
async fn anonymously_a_served_repository_is_read_with_the_token_and_a_pinned_action_without_it() {
    use crate::archives::{Budget, Source as _};
    let (url, seen) = archive_github().await;
    let github = anonymous_client(&url);
    let served = github.served_repository("o/app").await.unwrap();
    assert!(!served.public, "o/app is private");
    let authenticated = github.requests().rate_limit();
    assert_eq!(authenticated.map(|limit| limit.remaining), Some(4000));

    let action = github.action_repository("actions/checkout").await.unwrap();
    assert!(action.public);
    assert_eq!(
        github.action_repository("actions/checkout").await.unwrap(),
        action,
        "an unchanged answer is a 304 served from the anonymous reads' own kept answer"
    );
    let moved = github.action_repository("o/moved").await.unwrap_err();
    assert!(
        !moved.is_gone(),
        "a redirect is not a disappearance: {moved}"
    );
    let private = github.action_repository("o/private").await.unwrap_err();
    assert!(private.is_gone(), "not there to anyone: {private}");

    let seen = seen.lock().unwrap();
    for request in seen.iter() {
        let bearer = request.headers.get("authorization");
        if request.uri == "/repos/o/app" {
            assert!(bearer.is_some(), "the served read carries the token");
        } else {
            assert!(bearer.is_none(), "{} carries no credential", request.uri);
        }
    }
    assert!(
        seen.iter().all(|request| request.uri != "/repos/o/new"),
        "the redirect is not followed"
    );
    assert_eq!(
        seen.iter()
            .filter(|request| request.headers.contains_key("if-none-match"))
            .count(),
        1,
        "the second read of actions/checkout is conditional"
    );
    drop(seen);
    assert_eq!(
        tally_of(&github, Class::Repository),
        [1, 0, 0],
        "only the served read is counted with the token's"
    );
    assert_eq!(github.requests().rate_limit(), authenticated);
    assert_eq!(github.spare_requests(Budget::Public), None);
    assert_eq!(github.spare_requests(Budget::Served), Some(4000));
}

#[tokio::test]
async fn an_anonymous_rate_limit_holds_the_anonymous_reads_alone_and_is_no_disappearance() {
    use crate::archives::Source as _;
    let (url, seen) = archive_github().await;
    let github = anonymous_client(&url);
    github.served_repository("o/app").await.unwrap();
    let spend = github.spend();
    let report = github.requests();

    let limited = github.action_repository("o/limited").await.unwrap_err();
    assert!(
        matches!(limited, Error::RateLimited { .. }),
        "a used-up limit, not a refusal: {limited}"
    );
    assert!(!limited.is_gone());
    let held = github
        .action_repository("actions/checkout")
        .await
        .unwrap_err();
    assert!(matches!(held, Error::RateLimited { .. }), "{held}");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|request| request.uri != "/repos/actions/checkout"),
        "nothing is sent until the anonymous limit resets"
    );

    assert_eq!(
        github.blocked_until(),
        None,
        "the token's reads are not held"
    );
    assert_eq!(github.spend(), spend);
    assert_eq!(github.requests().rate_limit(), report.rate_limit());
    assert_eq!(tally_of(&github, Class::Repository), [1, 0, 0]);
    assert!(
        github.served_repository("o/app").await.is_ok(),
        "served reads go on"
    );
}

#[tokio::test]
async fn the_anonymous_reads_stop_at_their_hourly_cap_counting_every_send_304s_included() {
    use crate::archives::Source as _;
    let (url, seen) = archive_github().await;
    let github = anonymous_client(&url);
    for nth in 0..super::public::HOURLY_CAP {
        assert!(
            github.action_repository("actions/checkout").await.is_ok(),
            "read {nth} is within the cap"
        );
    }
    let capped = github
        .action_repository("actions/checkout")
        .await
        .unwrap_err();
    assert!(matches!(capped, Error::Held(_)), "{capped}");
    assert!(!capped.is_gone());
    assert_eq!(seen.lock().unwrap().len(), super::public::HOURLY_CAP);
    assert!(
        github.requests().is_empty(),
        "none counted with the token's"
    );
}

#[tokio::test]
async fn with_authenticated_public_reads_a_pinned_action_is_read_with_the_token_as_before() {
    use crate::archives::{Budget, Source as _};
    let (url, seen) = fake_github(|request| {
        let mut response = json(
            r#"{"full_name":"actions/checkout","visibility":"public","default_branch":"main"}"#,
        );
        let headers = response.headers_mut();
        headers.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("4999"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("4000000000"));
        assert!(request.headers.contains_key("authorization"));
        response
    })
    .await;
    let github = client(&url);
    assert!(
        github
            .action_repository("actions/checkout")
            .await
            .unwrap()
            .public
    );
    assert!(
        seen.lock().unwrap()[0]
            .headers
            .contains_key("authorization")
    );
    assert_eq!(tally_of(&github, Class::Repository), [1, 0, 0]);
    assert_eq!(github.spare_requests(Budget::Public), Some(4999));
    assert_eq!(github.spare_requests(Budget::Served), Some(4999));
}

#[tokio::test]
async fn a_token_that_expires_while_it_is_minted_is_a_failed_read_waited_out() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let sleeper = clock.clone();
    let tokens = expiring_tokens(
        &clock,
        minting(&clock, &mints, None, move |read, wall| {
            // The host sleeps two hours as GitHub answers: the token is already past its end.
            if read == 1 {
                sleeper.jump_wall(Duration::from_hours(2));
            }
            hourly(read, wall)
        }),
    );
    let failed = value(&tokens, None).await.unwrap_err();
    assert!(matches!(failed, Error::Token(_)), "{failed}");
    assert!(
        value(&tokens, None).await.is_err(),
        "within the backoff, no read and no expired token"
    );
    assert_eq!(mints.load(Ordering::SeqCst), 1);
    clock.pass(BACKOFF_FIRST);
    assert_eq!(value(&tokens, None).await.unwrap(), "t2");
}

#[tokio::test]
async fn a_token_that_expires_between_its_read_and_its_waiter_is_not_handed_over() {
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let tokens = expiring_tokens(&clock, minting(&clock, &mints, Some(gate.clone()), hourly));
    let mut waiting = Box::pin(value(&tokens, None));
    assert!(
        futures_util::poll!(waiting.as_mut()).is_pending(),
        "the read runs"
    );
    gate.add_permits(1);
    // The read ends and keeps t1, still usable; then the host sleeps before the waiter wakes.
    tokio::time::sleep(Duration::from_millis(20)).await;
    clock.jump_wall(Duration::from_hours(2));
    let late = waiting.await.unwrap_err();
    assert!(matches!(late, Error::Token(_)), "{late}");
    gate.add_permits(1);
    assert_eq!(
        value(&tokens, None).await.unwrap(),
        "t2",
        "the next request reads again"
    );
}

/// A token that expires while the permit is asked is not sent: a fresh one is read and the permit asked again.
#[tokio::test]
async fn a_token_that_expires_while_the_permit_is_asked_is_read_again_and_the_permit_asked_again() {
    let (url, seen) = fake_github(|_| StatusCode::ACCEPTED.into_response()).await;
    let clock = FakeClock::new();
    let mints = Arc::new(AtomicUsize::new(0));
    let github = Github::with_credentials(
        &url,
        Credentials::Expiring(minting(&clock, &mints, None, hourly)),
        PublicReads::Authenticated,
    )
    .unwrap()
    .with_token_clock(clock.clock());
    let asked = AtomicUsize::new(0);
    let permit = || -> BoxFuture<'_, bool> {
        // The first ask takes as long as the token had left.
        if asked.fetch_add(1, Ordering::SeqCst) == 0 {
            clock.pass(Duration::from_hours(1));
        }
        Box::pin(async { true })
    };
    assert_eq!(
        github
            .job_action("o/r", JobActionRequest::CancelRun { run: 1 }, &permit)
            .await,
        Ok(202)
    );
    assert_eq!(asked.load(Ordering::SeqCst), 2, "asked again with t2");
    let bearers: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|seen| seen.headers["authorization"].to_str().unwrap().to_owned())
        .collect();
    assert_eq!(bearers, ["Bearer t2"], "t1 never went out");
}

/// A 429 without a time to wait holds the anonymous reads for the default cooldown, and an ambiguous 403 too; neither
/// is a disappearance, and neither holds the token's reads.
#[tokio::test]
async fn an_anonymous_refusal_without_a_time_to_wait_is_cooled_down_alone() {
    use crate::archives::Source as _;
    for (status, limited) in [
        (StatusCode::TOO_MANY_REQUESTS, true),
        (StatusCode::FORBIDDEN, false),
    ] {
        let (url, seen) = fake_github(move |request| {
            if request.headers.contains_key("authorization") {
                let mut response =
                    json(r#"{"full_name":"o/app","visibility":"private","default_branch":"main"}"#);
                let headers = response.headers_mut();
                headers.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
                headers.insert("x-ratelimit-remaining", HeaderValue::from_static("4000"));
                headers.insert("x-ratelimit-reset", HeaderValue::from_static("4000000000"));
                response
            } else {
                status.into_response()
            }
        })
        .await;
        let github = anonymous_client(&url);
        github.served_repository("o/app").await.unwrap();
        let spend = github.spend();
        let before = SystemTime::now();
        let refused = github.action_repository("o/x").await.unwrap_err();
        assert!(!refused.is_gone(), "{status}: {refused}");
        assert_eq!(
            matches!(refused, Error::RateLimited { .. }),
            limited,
            "{status}: {refused}"
        );
        let held = github.action_repository("o/y").await.unwrap_err();
        let Error::RateLimited { until } = held else {
            panic!("{status}: held for the cooldown, not {held}");
        };
        assert!(
            until >= before + super::public::COOLDOWN
                && until <= SystemTime::now() + super::public::COOLDOWN,
            "{status}: held a minute"
        );
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|request| !request.headers.contains_key("authorization"))
                .count(),
            1,
            "{status}: nothing more sent during the cooldown"
        );
        assert_eq!(
            github.blocked_until(),
            None,
            "{status}: the token's reads go on"
        );
        assert_eq!(github.spend(), spend);
        assert!(github.served_repository("o/app").await.is_ok());
    }
}
