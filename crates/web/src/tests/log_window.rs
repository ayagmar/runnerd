//! A cited log line (ADR 0178): a failure's link and a search hit more lines before a log's end than the page outlines
//! there open the lines around them, folds and all; a line not read says why in core's words and is never marked
//! opened; an older daemon says it cannot read around the line, apart from missing evidence; and a running job's
//! cited lines are a snapshot its stream leaves alone. A cited `##[endgroup]` shows; long lines that stop the reads
//! short are read again from the line; and line 0 is answered, not ignored.

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
    let expired = logged(LOG_EXPIRED).outline.lines;
    // Past the end; cut at `history.log_max` before it; deleted after `history.logs`, at, at the last and past the
    // lines the deleted log had.
    for (id, line) in [
        (LOG_LONG, 999_999),
        (LOG_TRUNCATED, 30),
        (LOG_EXPIRED, 3),
        (LOG_EXPIRED, expired),
        (LOG_EXPIRED, expired + 1),
        (LOG_EXPIRED, 999_999),
    ] {
        let outline = logged(id).outline;
        let html = dashboard
            .page(&cookie, &format!("/jobs/{id}?line={line}"))
            .await
            .body;
        let section = log_of(&html);
        let why = sentence(&outline.absent_words(line - 1));
        if id == LOG_EXPIRED {
            assert!(
                why.contains("deleted"),
                "a deleted log reads as deleted: {why}"
            );
        }
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
        "a link to the full log"
    );
    // A line the client cannot know the log has: it is not said to have it, nor to lack it.
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line=999999"))
        .await
        .body;
    let shown = text(log_of(&html));
    let unread = sentence(&LogOutline::unread_words(999_998));
    assert!(shown.contains(&unread), "{unread}: {shown}");
    let past = sentence(&long_logged_job().outline.absent_words(999_998));
    assert!(
        !shown.contains(&past) && !shown.contains("has it"),
        "nothing said of whether the log has it: {shown}"
    );
}

#[tokio::test]
async fn a_daemon_with_the_outline_but_not_its_lines_reads_as_older_not_as_an_error() {
    let dashboard = Dashboard::start().await;
    dashboard.fake.fixtures.lock().unwrap().styled_lines = super::fake::StyledPages::Unsupported;
    let cookie = dashboard.sign_in().await;
    for path in [
        format!("/jobs/{LOG_LONG}"),
        format!("/jobs/{LOG_LONG}?line={}", LOG_LONG_HIT + 1),
    ] {
        let reply = dashboard.page(&cookie, &path).await;
        assert_eq!(
            reply.status,
            axum::http::StatusCode::OK,
            "{path}: {}",
            reply.body
        );
        let section = log_of(&reply.body);
        assert!(
            text(section).contains(&sentence(&Feature::LogOutline.unsupported())),
            "{path}: {}",
            text(section)
        );
        assert!(
            section.contains(r#"id="log-lines""#),
            "{path}: the plain tail shows: {section}"
        );
        assert!(!section.contains(MARKED), "{path}: nothing opened");
    }
    let html = dashboard
        .page(
            &cookie,
            &format!("/jobs/{LOG_LONG}?line={}", LOG_LONG_HIT + 1),
        )
        .await
        .body;
    let section = log_of(&html);
    assert!(
        text(section).contains(&sentence(&LogOutline::unread_words(LOG_LONG_HIT)))
            && section.contains(&format!(r#"href="/jobs/{LOG_LONG}#log""#)),
        "the line is not opened, with the way back: {section}"
    );
}

#[tokio::test]
async fn a_cited_closing_group_line_shows_marked_in_its_open_folds() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let long = long_logged_job();
    let closing = |start: u64| {
        long.outline
            .groups
            .iter()
            .find(|group| group.start == start)
            .and_then(|group| group.end)
            .unwrap()
    };
    let nested: Vec<u64> = long
        .outline
        .groups
        .iter()
        .filter(|group| group.start < LOG_LONG_HIT && group.end > Some(LOG_LONG_HIT))
        .map(|group| group.start)
        .collect();
    let [outer, inner] = nested[..] else {
        panic!("two groups around the hit: {nested:?}")
    };
    // The inner group's end opens both groups; the outer's, the outer.
    for (end, open) in [
        (closing(inner), vec![outer, inner]),
        (closing(outer), vec![outer]),
    ] {
        let html = dashboard
            .page(&cookie, &format!("/jobs/{LOG_LONG}?line={}", end + 1))
            .await
            .body;
        let section = log_of(&html);
        assert!(
            section.contains(&format!("{} {MARKED}", line_id(end))),
            "line {}: the closing line shows, marked: {section}",
            end + 1
        );
        for start in open {
            let fold = format!(r#"<li class="group" {}><details open>"#, line_id(start));
            assert!(section.contains(&fold), "line {}: {fold}", end + 1);
        }
    }
    // The cleanup's group starts more than 5000 lines before its end, which the log's last lines hold.
    let cleanup = long
        .outline
        .groups
        .iter()
        .find(|group| group.title.contains("Prune"))
        .unwrap();
    let end = cleanup.end.unwrap();
    assert!(end - cleanup.start > OUTLINED, "{cleanup:?}");
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line={}", end + 1))
        .await
        .body;
    let section = log_of(&html);
    assert!(
        section.contains(&format!("{} {MARKED}", line_id(end)))
            && !section.contains(&line_id(cleanup.start)),
        "a closing line whose group starts before the lines read shows, marked: {section}"
    );
}

#[tokio::test]
async fn long_lines_that_stop_the_reads_short_are_read_again_from_the_cited_line() {
    let dashboard = Dashboard::start().await;
    // A page of styled lines stops after its first line, as one of long lines stops at a few MiB.
    dashboard.fake.fixtures.lock().unwrap().styled_lines = super::fake::StyledPages::CutAt(1);
    let cookie = dashboard.sign_in().await;
    let long = long_logged_job();
    let failed = long.outline.step_lines(2).unwrap().to - 1;
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line={}", failed + 1))
        .await
        .body;
    let section = log_of(&html);
    assert!(
        section.contains(&format!("{} class=\"error\" {MARKED}", line_id(failed))),
        "{section}"
    );
    // Four reads of a line each, from the cited line: the window holds those four and says so.
    let read: Vec<u64> = (0..long.outline.lines)
        .filter(|line| section.contains(&line_id(*line)))
        .collect();
    assert_eq!(
        read,
        (failed..failed + 4).collect::<Vec<_>>(),
        "bounded, from the line"
    );
    let window = LogWindow::Around {
        line: failed,
        range: runnerd_core::api::LineRange {
            from: failed,
            to: long.outline.window(Some(failed), OUTLINED).range().to,
        },
    };
    let said = sentence(&window.words(4, long.outline.lines).unwrap());
    assert!(
        said.starts_with(&format!("Lines {} to {} of", failed + 1, failed + 4)),
        "{said}"
    );
    assert!(text(section).contains(&said), "{said}: {}", text(section));
}

#[tokio::test]
async fn a_read_again_that_finds_nothing_says_the_line_is_absent_and_shows_the_end() {
    let dashboard = Dashboard::start().await;
    let mut long = long_logged_job();
    let failed = long.outline.step_lines(2).unwrap().to - 1;
    // The outline counts every line; the store has only the first few, the cited one not among them.
    long.lines.truncate(usize::try_from(failed).unwrap() - 2);
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        fixtures.styled_lines = super::fake::StyledPages::CutAt(1);
        fixtures.logged.insert(LOG_LONG, long.clone());
    }
    let cookie = dashboard.sign_in().await;
    let html = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line={}", failed + 1))
        .await
        .body;
    let section = log_of(&html);
    let why = sentence(&long.outline.absent_words(failed));
    assert!(text(section).contains(&why), "{why}: {}", text(section));
    assert!(
        !section.contains(MARKED)
            && !section.contains("log-window")
            && !text(section).contains("around line"),
        "no window around nothing: {section}"
    );
}

#[tokio::test]
async fn line_zero_is_answered_never_read_as_no_line() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    let zero = sentence(LogOutline::zero_line_words());
    for id in [LOG_LONG, LEDGER_RUNNING] {
        let html = dashboard
            .page(&cookie, &format!("/jobs/{id}?line=0"))
            .await
            .body;
        let section = log_of(&html);
        assert!(text(section).contains(&zero), "job {id}: {}", text(section));
        assert!(
            section.contains(r#"role="alert""#)
                && section.contains(&format!(r#"href="/jobs/{id}#log""#)),
            "job {id}: an alert with a way back: {section}"
        );
        assert!(!section.contains(MARKED), "job {id}: nothing opened");
    }
    let running = dashboard
        .page(&cookie, &format!("/jobs/{LEDGER_RUNNING}?line=0"))
        .await
        .body;
    assert!(
        !running.contains("data-follow")
            && running.contains(&format!("/live/jobs/{LEDGER_RUNNING}?line=0")),
        "a running job's page cites line 0 to its stream too, its log not followed"
    );
    dashboard.fake.fixtures.lock().unwrap().older = true;
    let older = dashboard
        .page(&cookie, &format!("/jobs/{LOG_LONG}?line=0"))
        .await
        .body;
    assert!(
        text(log_of(&older)).contains(&zero),
        "an older daemon says the same"
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
