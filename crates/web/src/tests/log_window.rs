//! A cited log line (ADR 0178): a failure's link and a search hit more lines before a log's end than the page outlines
//! there open the lines around them, folds and all; a line not read says why in core's words and is never marked
//! opened; an older daemon says it cannot read around the line, apart from missing evidence; and a running job's
//! cited lines are a snapshot its stream leaves alone.

use runnerd_core::api::{Event, EventEnvelope, Feature, JobState, LogOutline, LogWindow};
use runnerd_fixtures::{
    LOG_EXPIRED, LOG_LONG, LOG_LONG_HIT, LOG_TRUNCATED, LoggedJob, logged_jobs, long_logged_job,
};

use super::parity::text;
use super::{Dashboard, LEDGER_RUNNING, NOW, read_until};
use crate::format::sentence;

/// The lines the page outlines at most; core's window is this many.
const OUTLINED: u64 = 5000;
/// The same, as many lines read.
const SHOWN: usize = 5000;

const MARKED: &str = r#"aria-current="true""#;

fn line_id(line: u64) -> String {
    format!(r#"id="L{}""#, line + 1)
}

fn logged(id: u64) -> LoggedJob {
    logged_jobs()
        .into_iter()
        .chain([long_logged_job()])
        .find(|logged| logged.job.summary.id == id)
        .unwrap()
}

/// The page's log section, from its title on.
fn log_of(html: &str) -> &str {
    &html[html.find(r#"id="log-title""#).unwrap()..]
}

#[tokio::test]
async fn the_default_page_of_a_long_log_outlines_its_last_lines_as_before() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let long = long_logged_job();
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}"))
        .await
        .body;
    let section = log_of(&html);
    let tail = long.outline.window(None, OUTLINED);
    let words = tail.words(SHOWN, long.outline.lines).unwrap();
    assert!(
        text(section).contains(&sentence(&words)),
        "{words}: {}",
        text(section)
    );
    assert!(
        section.contains(&line_id(long.outline.lines - 1))
            && !section.contains(&line_id(LOG_LONG_HIT)),
        "the log's end, not its start"
    );
    assert!(
        !section.contains("log-end")
            && !section.contains("log-window")
            && !section.contains(MARKED),
        "no window, no way back and nothing marked without a cited line: {section}"
    );
}

#[tokio::test]
async fn a_failure_link_before_the_last_lines_opens_the_window_holding_the_failed_step() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let long = long_logged_job();
    let failed = long.outline.step_lines(2).unwrap();
    let last = failed.to - 1;
    assert!(
        long.outline.lines - last > OUTLINED,
        "the failed step is before the lines the page outlines"
    );
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}"))
        .await
        .body;
    let link = format!(r#"href="?line={0}#L{0}""#, last + 1);
    let summary =
        &html[html.find(r#"id="failure""#).unwrap()..html.find(r#"id="log-title""#).unwrap()];
    assert!(
        summary.contains(&link),
        "the step's link names its line: {summary}"
    );

    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line={}", last + 1))
        .await
        .body;
    let section = log_of(&html);
    let marked = format!(r#"{} class="error" {MARKED}"#, line_id(last));
    assert!(section.contains(&marked), "{marked}: {section}");
    assert!(
        section.contains(r#"<details class="log-step" open data-scroll-here"#),
        "the failed step's fold opens, scrolled to: {section}"
    );
    let window = long.outline.window(Some(last), OUTLINED);
    assert!(
        matches!(window, LogWindow::Around { .. }),
        "core reads around it: {window:?}"
    );
    let said = window.words(SHOWN, long.outline.lines).unwrap();
    assert!(
        text(section).contains(&sentence(&said)),
        "{said}: {}",
        text(section)
    );
    assert!(
        section.contains(&format!(r#"href="/jobs/{LOG_LONG}#log""#))
            && text(section).contains("Back to the log's end"),
        "a way back to the end: {section}"
    );
}

#[tokio::test]
async fn a_search_hit_two_groups_deep_before_the_last_lines_opens_both_groups() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let long = long_logged_job();
    let hit = LOG_LONG_HIT + 1;
    let found = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?q=balances"))
        .await
        .body;
    assert!(
        found.contains(&format!(r#"href="?line={hit}#L{hit}""#)),
        "the hit links to its line: {found}"
    );
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line={hit}"))
        .await
        .body;
    let section = log_of(&html);
    assert!(
        section.contains(&format!("{} {MARKED}", line_id(LOG_LONG_HIT))),
        "{section}"
    );
    let groups: Vec<u64> = long
        .outline
        .groups
        .iter()
        .filter(|group| group.start < LOG_LONG_HIT && group.end > Some(LOG_LONG_HIT))
        .map(|group| group.start)
        .collect();
    assert_eq!(groups.len(), 2, "{groups:?}");
    for start in groups {
        let open = format!(r#"<li class="group" {}><details open>"#, line_id(start));
        assert!(section.contains(&open), "{open}: {section}");
    }
    assert!(
        !section.contains(&line_id(long.outline.lines - 1)),
        "the window is bounded: the log's end is not read"
    );
}

#[tokio::test]
async fn a_cited_line_the_log_lacks_says_why_and_opens_nothing() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    // Past the end; cut at `history.log_max` before it; deleted after `history.logs`.
    for (id, line) in [(LOG_LONG, 999_999), (LOG_TRUNCATED, 30), (LOG_EXPIRED, 3)] {
        let outline = logged(id).outline;
        let html = dashboard
            .page(&cookie, &format!("/jobs/{id}?line={line}"))
            .await
            .body;
        let section = log_of(&html);
        let why = sentence(&outline.absent_words(line - 1));
        assert!(
            text(section).contains(&why),
            "job {id}: {why}: {}",
            text(section)
        );
        assert!(
            section.contains(r#"role="alert""#)
                && section.contains(&format!(r#"href="/jobs/{id}#log""#)),
            "job {id}: an alert with a way back to the end: {section}"
        );
        assert!(
            !html.contains(MARKED),
            "job {id}: no line is marked as opened"
        );
        assert!(
            !text(section).contains(&sentence(&LogOutline::unread_words(line - 1))),
            "job {id}: missing evidence is not an older daemon"
        );
    }
    // The page falls back to the log's end.
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line=999999"))
        .await
        .body;
    assert!(html.contains(&line_id(long_logged_job().outline.lines - 1)));
}

#[tokio::test]
async fn an_older_daemon_says_it_cannot_read_around_a_cited_line_not_that_the_line_is_missing() {
    let dashboard = Dashboard::start().await;
    dashboard.fake.fixtures.lock().unwrap().older = true;
    let cookie = dashboard.sign_in().await;
    let hit = LOG_LONG_HIT + 1;
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line={hit}"))
        .await
        .body;
    let shown = text(log_of(&html));
    for said in [
        sentence(&Feature::LogOutline.unsupported()),
        sentence(&LogOutline::unread_words(LOG_LONG_HIT)),
    ] {
        assert!(shown.contains(&said), "{said}: {shown}");
    }
    let missing = sentence(&long_logged_job().outline.absent_words(LOG_LONG_HIT));
    assert!(!shown.contains(&missing), "not missing evidence: {shown}");
    assert!(!html.contains(MARKED), "no line is marked as opened");
    assert!(
        html.contains(&format!(r#"href="/jobs/{LOG_LONG}/log""#)),
        "the full log has it"
    );
}

#[tokio::test]
async fn a_running_jobs_cited_line_is_a_snapshot_the_stream_leaves_alone_until_the_job_ends() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let lines = dashboard.fake.fixtures.lock().unwrap().logs[&LEDGER_RUNNING].len() as u64;
    let page = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}?line=1"))
        .await
        .body;
    let section = log_of(&page);
    assert!(
        section.contains(&format!(r#"<li class="group" {} {MARKED}>"#, line_id(0))),
        "{section}"
    );
    assert!(
        !page.contains("data-follow") && !page.contains(r#"id="log-lines""#),
        "the cited lines are not followed: {section}"
    );
    assert!(
        text(section).contains("Back to the live tail")
            && section.contains(&format!(r#"href="/jobs/{LEDGER_RUNNING}#log""#)),
        "{section}"
    );
    assert!(
        page.contains(&format!("/live/jobs/{LEDGER_RUNNING}?line=1")),
        "the stream knows the page cites a line"
    );

    let mut stream = dashboard
        .stream(&cookie, &format!("/live/jobs/{LEDGER_RUNNING}?line=1"))
        .await;
    let mut sent = read_until(&mut stream, |text| text.contains(r#"id="metrics""#)).await;
    let summary = {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        let job = fixtures
            .jobs
            .iter_mut()
            .find(|job| job.summary.id == LEDGER_RUNNING)
            .unwrap();
        job.summary.state = JobState::Completed;
        job.summary.conclusion = Some("success".to_owned());
        job.summary.timeline.finished_at = Some(NOW + 2);
        job.summary.clone()
    };
    for event in [
        Event::LogLine {
            job: LEDGER_RUNNING,
            line: lines,
            text: "a line after the snapshot".to_owned(),
        },
        Event::JobFinished { job: summary },
    ] {
        dashboard
            .fake
            .events
            .send(EventEnvelope {
                id: None,
                at: NOW + 2,
                event,
            })
            .unwrap();
    }
    sent.push_str(&read_until(&mut stream, |text| text.contains(r#"id="compare""#)).await);
    assert!(
        !sent.contains("#log-lines") && !sent.contains("a line after the snapshot"),
        "no line is appended to the cited lines: {sent}"
    );
    let finished = &sent[sent.rfind(r#"id="failure""#).unwrap()..];
    assert!(
        finished.contains(&format!(r#"<li class="group" {} {MARKED}>"#, line_id(0))),
        "the finished log keeps the cited line open: {finished}"
    );
}
