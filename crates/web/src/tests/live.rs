//! A live page's feed (ADR 0177): a read that fails as the daemon is away is tried again on the same stream, the page
//! keeping what it shows, and a stream that ends for good says why instead of claiming to retry.

use std::time::Duration;

use http_body_util::BodyExt;
use runnerd_core::api::{Event, EventEnvelope, HostSample, JobState, Sample, Step};

use super::{
    CURRENT, DELIVERED, Dashboard, LEDGER_RUNNING, NOW, ShownLog, page_signals, query, read_until,
    signals_of,
};

impl Dashboard {
    fn send_event(&self, event: Event) {
        self.fake
            .events
            .send(EventEnvelope {
                id: None,
                at: NOW + 2,
                event,
            })
            .unwrap();
    }

    /// Cuts off the next `count` reads of `path` mid-body, as when the daemon goes away while it answers.
    pub(super) fn drop_reads(&self, path: &str, count: u32) {
        self.fake
            .fixtures
            .lock()
            .unwrap()
            .drops
            .insert(path.to_owned(), count);
    }

    /// A change of the budget, which makes the overview read the status again.
    fn budget_changed(&self) {
        let budget = self.fake.fixtures.lock().unwrap().status.budget.clone();
        self.send_event(Event::BudgetChanged { budget });
    }
}

/// The `_feed` signals in `text`, in the order they came.
fn feeds(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: signals "))
        .filter_map(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .filter_map(|signals| signals.get("_feed").cloned())
        .collect()
}

pub(super) fn states(text: &str) -> Vec<String> {
    feeds(text)
        .iter()
        .map(|feed| feed["state"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// How a stream ends, and what it sent until then: `ended` cleanly, which Datastar does not retry, or `failed`.
async fn to_end(stream: &mut hyper::body::Incoming) -> (String, &'static str) {
    let mut text = String::new();
    let end = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.frame().await {
                Some(Ok(frame)) => {
                    text.push_str(&String::from_utf8_lossy(
                        &frame.into_data().unwrap_or_default(),
                    ));
                }
                Some(Err(_)) => return "failed",
                None => return "ended",
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the stream did not end: {text}"));
    (text, end)
}

/// A dropped status read ended the overview's stream with a notice, though the daemon answered the next one.
#[tokio::test]
async fn an_overview_read_that_drops_after_an_event_is_retried_on_the_same_stream_keeping_the_page()
{
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    let first = read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    assert_eq!(states(&first), ["awaiting", "current"], "{first}");
    let delivered = feeds(&first)[1]["at"].as_u64().unwrap();
    dashboard.drop_reads("/v1/status", 2);
    dashboard.budget_changed();
    let recovery = read_until(&mut stream, |text| text.contains(CURRENT)).await;
    assert_eq!(
        states(&recovery),
        ["retrying", "retrying", "current"],
        "{recovery}"
    );
    let retrying = &feeds(&recovery)[0];
    assert_eq!(
        retrying["at"].as_u64(),
        Some(delivered),
        "a retry keeps when the page's data is from: {retrying}"
    );
    assert_eq!(retrying["ended"], false);
    let before_recovered = &recovery[..recovery.rfind("data: signals").unwrap()];
    assert!(
        !before_recovered.contains("datastar-patch-elements"),
        "nothing the page shows is replaced while the read fails: {recovery}"
    );
    assert!(!recovery.contains("toast failed"), "{recovery}");
    // The same stream goes on.
    let sample = HostSample {
        at: NOW + 3,
        cpu: 31_000,
        memory: 21 << 30,
    };
    dashboard
        .fake
        .fixtures
        .lock()
        .unwrap()
        .host
        .push(sample.clone());
    dashboard.send_event(Event::HostSample { sample });
    let update = read_until(&mut stream, |text| text.contains(r#"id="host-cpu-now""#)).await;
    assert!(update.contains("31 cores"), "{update}");
}

/// A page opened while the daemon dropped its first read ended at once, its badge saying it retried.
#[tokio::test]
async fn a_first_read_that_drops_is_retried_until_the_page_gets_its_data() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    dashboard.drop_reads("/v1/status", 1);
    let mut stream = dashboard.stream(&cookie, "/live/queue").await;
    let first = read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    assert_eq!(
        states(&first),
        ["awaiting", "retrying", "current"],
        "{first}"
    );
    assert_eq!(
        feeds(&first)[1]["at"],
        0,
        "no data was delivered before: {first}"
    );
    assert!(first.contains(r#"id="waiting""#), "{first}");
}

/// A recovered job page sent its log's tail again, or numbered the next line from the start.
#[tokio::test]
async fn a_job_read_that_drops_keeps_the_log_where_it_was_and_goes_on_from_it() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let (page, url) = dashboard.job_page(&cookie).await;
    let mut shown = ShownLog::of_page(&page);
    let mut stream = dashboard
        .stream(&cookie, &format!("{url}?{}", signals_of(&page)))
        .await;
    read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    dashboard.log("before the drop");
    // The line, then the delivery it is part of.
    shown.apply(
        &read_until(&mut stream, |text| {
            text.split_once("before the drop")
                .is_some_and(|(_, after)| after.contains(DELIVERED))
        })
        .await,
    );
    dashboard.drop_reads(&format!("/v1/jobs/{LEDGER_RUNNING}"), 2);
    dashboard.send_event(Event::JobStep {
        job: LEDGER_RUNNING,
        step: Step {
            name: "Build".to_owned(),
            number: 3,
            status: "in_progress".to_owned(),
            conclusion: None,
            started_at: None,
            completed_at: None,
        },
    });
    let recovery = read_until(&mut stream, |text| text.contains(CURRENT)).await;
    assert_eq!(
        states(&recovery),
        ["retrying", "retrying", "current"],
        "{recovery}"
    );
    dashboard.log("after the recovery");
    let after = read_until(&mut stream, |text| text.contains("after the recovery")).await;
    let all = format!("{recovery}{after}");
    assert!(
        !all.contains(r#"id="log-lines""#),
        "the log is appended to, not sent again: {all}"
    );
    shown.apply(&all);
    let texts: Vec<&str> = shown.0.iter().map(|line| line.1.as_str()).collect();
    assert_eq!(
        texts
            .iter()
            .filter(|text| **text == "before the drop")
            .count(),
        1,
        "{texts:?}"
    );
    assert_eq!(texts.last(), Some(&"after the recovery"));
    let numbers: Vec<Option<u64>> = shown.0.iter().rev().take(2).map(|line| line.0).collect();
    assert_eq!(
        numbers[0],
        numbers[1].map(|before| before + 1),
        "the next line is numbered after the last: {texts:?}"
    );
}

/// A retry that outlived a sign-out kept a signed-out browser's page reading the daemon.
#[tokio::test]
async fn signing_out_while_a_read_is_retried_ends_the_stream_without_another_try() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    dashboard.drop_reads("/v1/status", u32::MAX);
    dashboard.budget_changed();
    read_until(&mut stream, |text| text.contains(r#""state":"retrying""#)).await;
    dashboard.sessions.sign_out_all().unwrap();
    let (text, end) = to_end(&mut stream).await;
    assert_eq!(end, "ended", "a retry would be refused: {text}");
    assert!(text.contains("This browser was signed out"), "{text}");
    let last = feeds(&text).pop().unwrap();
    assert_eq!(last["state"], "signed-out", "{text}");
    assert_eq!(last["ended"], true);
    assert_eq!(
        last["reload"], true,
        "reloading leads to signing in: {last}"
    );
}

/// A finished job's page said "Offline; retrying" once its stream ended, or connected again.
#[tokio::test]
async fn a_finished_jobs_stream_ends_saying_it_finished_and_never_retries() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let finished = dashboard
        .fake
        .fixtures
        .lock()
        .unwrap()
        .jobs
        .iter()
        .find(|job| job.summary.state == JobState::Completed)
        .unwrap()
        .summary
        .id;
    let mut stream = dashboard
        .stream(&cookie, &format!("/live/jobs/{finished}"))
        .await;
    let (text, end) = to_end(&mut stream).await;
    assert_eq!(
        end, "ended",
        "a finished job is not connected again: {text}"
    );
    assert_eq!(states(&text).last().map(String::as_str), Some("finished"));
    assert!(!states(&text).contains(&"retrying".to_owned()), "{text}");
    assert_eq!(feeds(&text).last().unwrap()["reload"], false);
}

/// A daemon's refusal is shown and the badge says the page stopped updating; it never says it retries.
#[tokio::test]
async fn a_refused_read_ends_the_feed_as_refused_with_a_reload() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/jobs/999").await;
    let (text, end) = to_end(&mut stream).await;
    assert_eq!(end, "ended", "{text}");
    assert_eq!(states(&text), ["awaiting", "refused"], "{text}");
    assert_eq!(feeds(&text)[1]["reload"], true);
}

/// Taking a keep-alive for a delivery would make a page whose reads stopped look current.
#[tokio::test]
async fn a_keep_alive_delivers_nothing_and_leaves_when_the_data_is_from() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    // Keep-alives come every second in tests; no event comes, so nothing is read.
    let quiet = read_until(&mut stream, |text| {
        text.lines().filter(|line| line.starts_with(':')).count() >= 2
    })
    .await;
    assert!(
        feeds(&quiet).is_empty() && !quiet.contains(DELIVERED),
        "a keep-alive changes neither the feed nor the page: {quiet}"
    );
}

/// A daemon that denied the event reader's reopening made the page fail, so Datastar connected again only to be
/// refused, instead of ending with the refusal shown.
#[tokio::test]
async fn an_event_stream_the_daemon_denies_on_reopening_ends_the_page_without_a_reconnect() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    dashboard
        .fake
        .fixtures
        .lock()
        .unwrap()
        .denials
        .insert("/v1/events".to_owned(), 1);
    dashboard.fake.cut_events();
    let (text, end) = to_end(&mut stream).await;
    assert_eq!(
        end, "ended",
        "a denied reader is not connected again: {text}"
    );
    assert!(
        text.contains("toast failed"),
        "the refusal is shown: {text}"
    );
    assert_eq!(states(&text).last().map(String::as_str), Some("refused"));
}

/// An event the page ignores moved the time its data was from, so a later outage claimed newer data than the last
/// read.
#[tokio::test]
async fn an_event_the_page_ignores_leaves_when_its_data_is_from() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    // Sent at once, before the day's figures are a second old and read again.
    dashboard.send_event(Event::CacheReseeded {
        profile: "jvm".to_owned(),
        output: String::new(),
        error: None,
    });
    let tick = read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    assert!(
        feeds(&tick).is_empty(),
        "a render after an ignored event is no delivery: {tick}"
    );
}

/// A dropped read of the savings turned them into "none" and called the page live, instead of keeping them while
/// it read again.
#[tokio::test]
async fn a_dropped_read_of_the_savings_is_read_again() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    let finished = dashboard
        .fake
        .fixtures
        .lock()
        .unwrap()
        .jobs
        .iter()
        .find(|job| job.summary.state == JobState::Completed)
        .unwrap()
        .summary
        .clone();
    dashboard.drop_reads("/v1/savings", 1);
    dashboard.send_event(Event::JobFinished { job: finished });
    let recovery = read_until(&mut stream, |text| text.contains(CURRENT)).await;
    assert_eq!(states(&recovery), ["retrying", "current"], "{recovery}");
}

/// Applies the `shown` signal of each patch in `text` to `shown`, as the page keeps it.
fn apply_shown(shown: &mut serde_json::Value, text: &str) {
    for json in text
        .lines()
        .filter_map(|line| line.strip_prefix("data: signals "))
    {
        let Ok(signals) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        if let Some(patch) = signals.get("shown").and_then(serde_json::Value::as_object) {
            for (key, value) in patch {
                shown[key] = value.clone();
            }
        }
    }
}

/// A page that connected again after a render that only moved its clock said its data was from that render.
#[tokio::test]
async fn a_page_that_connects_again_after_a_render_that_delivered_nothing_keeps_when_its_data_is_from()
 {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let (page, url) = dashboard.job_page(&cookie).await;
    let mut shown = page_signals(&page)["shown"].clone();
    let mut stream = dashboard
        .stream(&cookie, &format!("{url}?{}", signals_of(&page)))
        .await;
    apply_shown(
        &mut shown,
        &read_until(&mut stream, |text| text.contains(DELIVERED)).await,
    );
    let fed = shown["fed"].as_u64().unwrap();
    // A second on, a line the page has already: the stream ignores it, and the render only moves the clock.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    dashboard.send_event(Event::LogLine {
        job: LEDGER_RUNNING,
        line: 0,
        text: "a line the page has".to_owned(),
    });
    let tick = read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    assert!(feeds(&tick).is_empty(), "{tick}");
    apply_shown(&mut shown, &tick);
    assert!(
        shown["at"].as_u64().unwrap() > fed,
        "the clock moved: {shown}"
    );
    drop(stream);
    // The page connects again with what it shows; its first read fails, so nothing new is delivered yet.
    dashboard.drop_reads(&format!("/v1/jobs/{LEDGER_RUNNING}"), 1);
    let retried = serde_json::json!({ "shown": shown });
    let mut again = dashboard
        .stream(&cookie, &format!("{url}?{}", query(&retried)))
        .await;
    let before = read_until(&mut again, |text| text.contains(r#""state":"retrying""#)).await;
    assert_eq!(states(&before), ["awaiting", "retrying"], "{before}");
    for feed in feeds(&before) {
        assert_eq!(
            feed["at"].as_u64(),
            Some(fed),
            "the data is from the last delivery, not the last render: {feed}"
        );
    }
}

/// A sample of a job the overview does not follow, which it drops, counted as a delivery.
#[tokio::test]
async fn a_sample_of_a_job_the_overview_does_not_follow_delivers_nothing() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/overview").await;
    read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    // Sent at once, before the day's figures are a second old and read again.
    dashboard.send_event(Event::JobSample {
        job: 999,
        sample: Sample {
            at: NOW + 2,
            cpu: 1000,
            memory: 1 << 30,
            network_received: 0,
            network_sent: 0,
        },
    });
    let tick = read_until(&mut stream, |text| text.contains(DELIVERED)).await;
    assert!(
        feeds(&tick).is_empty(),
        "a dropped sample is no delivery: {tick}"
    );
}
