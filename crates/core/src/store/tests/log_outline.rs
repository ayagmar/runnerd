//! Schema 22: a job's log outline, kept by the writer as lines are stored (ADR 0133), across restarts, the log cap,
//! pruning and a history an older runnerd wrote.

use rusqlite::Connection;

use super::{DAY_MS, queued, runner, user_version};
use crate::api::{
    self, LINE_BYTES_MAX, LineRead, LogEvidence, LogLinesQuery, LogOutline, LogSearchQuery,
    MarkKind, RUNS_MAX, Search, StepLog,
};
use crate::github::{JobDetails, JobStep};
use crate::store::{LogPosition, Store, VERSION};
use crate::testdir::TestDir;

/// The schema at version 21, as runnerd 0.0.23 to 0.1.0 created it.
const SCHEMA_21: &str = include_str!("schema-21.sql");

/// 2026-10-23T01:00:00Z, in seconds.
const HOUR: u64 = 1_792_717_200;

const POSITION: LogPosition = LogPosition { page: 1, offset: 0 };

fn at(second: u64, text: &str) -> String {
    format!("2026-10-23T01:00:{second:02}.5Z {text}")
}

/// A log with set-up groups, a step header, a nested group left open across a batch, a warning, a post step and a
/// failure.
fn log() -> Vec<String> {
    vec![
        at(0, "Current runner version: '2.330.0'"),
        at(0, "##[group]Operating System"),
        at(0, "Ubuntu"),
        at(1, "##[endgroup]"),
        at(2, "##[group]Run make test"),
        at(2, "\u{1b}[36;1mmake test\u{1b}[0m"),
        at(2, "##[endgroup]"),
        at(3, "##[group]Suite"),
        at(4, "##[group]Run unit"),
        at(5, "ok"),
        at(6, "##[endgroup]"),
        at(7, "##[warning]slow test"),
        at(8, "##[endgroup]"),
        at(9, "\u{1b}[31mFAILED\u{1b}[0m"),
        at(9, "##[error]Process completed with exit code 2."),
        at(11, "Post job cleanup."),
        at(11, "removed"),
    ]
}

fn step(number: u32, name: &str, start: u64, end: u64, conclusion: &str) -> JobStep {
    let time = |second: u64| Some(format!("2026-10-23T01:00:{second:02}Z"));
    JobStep {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: time(start),
        completed_at: time(end),
    }
}

/// Job `id`'s runner took it, as runner `r<id>`.
fn running(store: &Store, id: u64) {
    let name = format!("r{id}");
    store.queued("o/r", &queued(id), HOUR * 1000);
    store.runner_started(runner(&name, HOUR * 1000));
    store.claimed(id, 10, &name, HOUR * 1000);
}

/// GitHub reports job `id` concluded and its runner is gone: its record is final.
fn finish(store: &Store, id: u64, conclusion: &str) {
    let name = format!("r{id}");
    store.details(JobDetails {
        id,
        run_id: 10,
        run_attempt: 1,
        name: format!("job {id}"),
        workflow_name: None,
        head_branch: None,
        html_url: None,
        labels: Vec::new(),
        created_at: None,
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: Some("2026-10-23T01:00:00Z".to_owned()),
        completed_at: Some("2026-10-23T01:00:12Z".to_owned()),
        runner_name: Some(name.clone()),
        steps: vec![
            step(1, "Set up job", 0, 1, "success"),
            step(2, "Tests", 2, 9, conclusion),
            step(3, "Post Run actions/checkout@v4", 11, 11, "success"),
        ],
    });
    store.runner_removed(&name, (HOUR + 12) * 1000);
}

#[tokio::test]
async fn an_outline_kept_across_batches_and_restarts_is_the_whole_logs_outline() {
    let log = log();
    let dir = TestDir::new("outline");
    // Every place a batch can end, with a restart there, and the cut lines stored a line at a time after it.
    for cut in 0..=log.len() {
        let path = dir.0.join(format!("runnerd-{cut}.db"));
        let store = Store::open(&path).unwrap();
        running(&store, 1);
        store.log(1, log[..cut].to_vec(), POSITION, u64::MAX).await;
        store.flush().await;
        let outline = store.log_outline(1).await.unwrap().unwrap();
        assert_eq!(outline.evidence, LogEvidence::Running, "cut at {cut}");
        assert_eq!(outline.lines, cut as u64);
        drop(store);
        let store = Store::open(&path).unwrap();
        for line in &log[cut..] {
            store.log(1, vec![line.clone()], POSITION, u64::MAX).await;
        }
        finish(&store, 1, "failure");
        store.flush().await;
        let job = store.job(1).await.unwrap().unwrap();
        let outline = store.log_outline(1).await.unwrap().unwrap();
        assert_eq!(outline, LogOutline::of(&job, &log), "cut at {cut}");
    }
    let job = Store::open(&dir.0.join("runnerd-0.db"))
        .unwrap()
        .job(1)
        .await
        .unwrap()
        .unwrap();
    let outline = LogOutline::of(&job, &log);
    assert_eq!(outline.evidence, LogEvidence::Complete);
    assert_eq!(
        outline
            .steps
            .iter()
            .map(|step| step.lines)
            .collect::<Vec<_>>(),
        [
            StepLog::Lines(api::LineRange { from: 0, to: 4 }),
            StepLog::Lines(api::LineRange { from: 4, to: 15 }),
            StepLog::Lines(api::LineRange { from: 15, to: 17 }),
        ]
    );
    let failure = outline.failure.unwrap();
    assert_eq!((failure.step, failure.exit_code), (Some(2), Some(2)));
}

#[tokio::test]
async fn a_running_jobs_outline_shows_its_open_groups_and_grows() {
    let dir = TestDir::new("outline");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    running(&store, 1);
    store.log(1, log()[..10].to_vec(), POSITION, u64::MAX).await;
    store.flush().await;
    let outline = store.log_outline(1).await.unwrap().unwrap();
    let open: Vec<(u64, Option<u64>)> = outline
        .groups
        .iter()
        .map(|group| (group.start, group.end))
        .collect();
    assert_eq!(open, [(1, Some(3)), (4, Some(6)), (7, None), (8, None)]);
    assert_eq!(
        (outline.evidence, outline.failure),
        (LogEvidence::Running, None)
    );
    store.log(1, log()[10..].to_vec(), POSITION, u64::MAX).await;
    store.flush().await;
    let outline = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(
        outline.groups[2..],
        [
            api::LogGroup {
                start: 7,
                end: Some(12),
                depth: 0,
                title: "Suite".to_owned()
            },
            api::LogGroup {
                start: 8,
                end: Some(10),
                depth: 1,
                title: "Run unit".to_owned()
            },
        ]
    );
    assert_eq!(
        outline
            .marks
            .iter()
            .map(|mark| mark.kind)
            .collect::<Vec<_>>(),
        [MarkKind::Warning, MarkKind::Error]
    );
}

#[tokio::test]
async fn a_log_cut_at_its_cap_before_the_failure_leaves_the_exit_code_unknown() {
    let dir = TestDir::new("outline");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    running(&store, 1);
    let cap: u64 = log()[..10].iter().map(|line| line.len() as u64 + 1).sum();
    store.log(1, log(), POSITION, cap).await;
    finish(&store, 1, "failure");
    store.flush().await;
    let outline = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(
        (outline.evidence, outline.lines),
        (LogEvidence::Truncated, 10)
    );
    let failure = outline.failure.unwrap();
    assert_eq!(
        (failure.step, failure.exit_code),
        (Some(2), None),
        "the step is GitHub's; its exit code was past the cap"
    );
    assert_eq!(
        failure
            .excerpt
            .map(|lines| lines.last().map(|line| line.line)),
        Some(Some(9)),
        "its last lines are the last it kept"
    );
}

#[tokio::test]
async fn a_pruned_log_reads_as_expired_and_takes_its_outline_with_it() {
    let dir = TestDir::new("outline");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    running(&store, 1);
    store.log(1, log(), POSITION, u64::MAX).await;
    finish(&store, 1, "failure");
    store.flush().await;
    // Part way through a prune, which deletes from the first line.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM log_lines WHERE job = 1 AND line < 5", [])
        .unwrap();
    let outline = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(outline.evidence, LogEvidence::Expired);
    assert!(
        outline
            .steps
            .iter()
            .all(|step| step.lines == StepLog::Unattributed),
        "no step claims lines that are going"
    );
    assert_eq!(outline.failure.map(|failure| failure.exit_code), Some(None));

    store
        .prune(
            (HOUR + 12) * 1000 + 31 * DAY_MS,
            &crate::config::History::default(),
        )
        .await
        .unwrap();
    let left: u64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM log_outlines) + (SELECT count(*) FROM log_groups) \
             + (SELECT count(*) FROM log_marks)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "the outline goes with its lines");
    let outline = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(
        (outline.evidence, outline.groups.len()),
        (LogEvidence::Expired, 0)
    );
    let search = Search::new(&LogSearchQuery {
        q: "FAILED".to_owned(),
        ..LogSearchQuery::default()
    })
    .unwrap();
    assert_eq!(
        store.search_log(1, search).await.unwrap().matches,
        Vec::<u64>::new()
    );
}

#[tokio::test]
async fn a_failed_job_without_a_log_still_offers_its_kept_record() {
    let dir = TestDir::new("outline");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    running(&store, 1);
    finish(&store, 1, "failure");
    store.record_made(api::Record {
        job: 1,
        repository: "o/r".to_owned(),
        profile: "app".to_owned(),
        workflow: None,
        name: None,
        created_at: HOUR + 20,
        bytes: 1 << 30,
        image: "runnerd-record:1".to_owned(),
    });
    store.flush().await;
    let outline = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(outline.evidence, LogEvidence::Missing);
    let failure = outline.failure.unwrap();
    assert_eq!(
        (
            failure.step,
            failure.exit_code,
            failure.excerpt,
            failure.shell.as_deref()
        ),
        (Some(2), None, None, Some("runnerctl shell 1"))
    );
    assert_eq!(store.log_outline(2).await.unwrap(), None, "no such job");
}

/// A history at version 21 at `path`: job 1 finished with a log, one of its lines not UTF-8, and job 2 running with
/// three lines, both stored before runnerd kept outlines.
fn history_21(path: &std::path::Path) {
    let db = Connection::open(path).unwrap();
    db.execute_batch(SCHEMA_21).unwrap();
    db.execute_batch(
        "INSERT INTO runners (name, repository, profile, slot, container, started_at)
         VALUES ('r2', 'o/r', 'app', 1, 'c', 1000);
         INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, conclusion, queued_at,
         started_at, finished_at, log_lines, log_bytes)
         VALUES (1, 'o/r', 10, 1, 'build', '[]', 'completed', 'success', 1000, 2000, 3000, 3, 40);
         INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, runner, queued_at, started_at,
         log_lines, log_bytes)
         VALUES (2, 'o/r', 11, 1, 'test', '[]', 'running', 'r2', 1000, 2000, 3, 30);
         INSERT INTO log_lines (job, line, text) VALUES
         (1, 0, '##[group]Run make'), (1, 1, CAST(X'626164E920FF' AS TEXT)), (1, 2, '##[endgroup]'),
         (2, 0, '##[group]Run make'), (2, 1, 'one'), (2, 2, 'two');",
    )
    .unwrap();
    db.pragma_update(None, "user_version", 21).unwrap();
}

#[tokio::test]
async fn a_v21_history_opens_at_22_with_its_logs_readable_without_an_outline() {
    let dir = TestDir::new("outline-21");
    let path = dir.0.join("runnerd.db");
    history_21(&path);
    let store = Store::open(&path).unwrap();
    assert_eq!(user_version(&path), VERSION);
    let outline = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(outline.evidence, LogEvidence::NotOutlined);
    assert_eq!(
        (outline.from, outline.unattributed.as_slice()),
        (3, [api::LineRange { from: 0, to: 3 }].as_slice())
    );
    let lines = store.styled_lines(1, 0, 10).await.unwrap();
    assert_eq!(
        lines[1].text(),
        "bad\u{fffd} \u{fffd}",
        "a line that is not UTF-8 reads, replaced"
    );
    let search = Search::new(&LogSearchQuery {
        q: "bad".to_owned(),
        ..LogSearchQuery::default()
    })
    .unwrap();
    assert_eq!(store.search_log(1, search).await.unwrap().matches, [1]);

    // A job running across the upgrade is outlined from its next line.
    store
        .log(
            2,
            vec!["##[endgroup]".to_owned(), "##[group]Later".to_owned()],
            POSITION,
            u64::MAX,
        )
        .await;
    store.flush().await;
    let outline = store.log_outline(2).await.unwrap().unwrap();
    assert_eq!((outline.evidence, outline.from), (LogEvidence::Partial, 3));
    assert_eq!(
        outline
            .groups
            .iter()
            .map(|group| (group.start, group.end))
            .collect::<Vec<_>>(),
        [(4, None)],
        "an end whose group came before the outline closes nothing"
    );
}

#[tokio::test]
async fn a_long_log_of_short_lines_is_outlined_as_it_is_stored_and_read_within_bounds() {
    const LINE: usize = 1024;
    const LINES: u64 = 50 * 1024;
    let dir = TestDir::new("outline-big");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    running(&store, 1);
    let filler = "x".repeat(LINE - 40);
    for batch in 0..LINES / 1024 {
        let lines = (batch * 1024..(batch + 1) * 1024)
            .map(|line| match line % 4 {
                0 => format!("##[group]step {line}"),
                1 => format!("##[warning]{line} {filler}"),
                2 => "##[endgroup]".to_owned(),
                _ => format!("{line} {filler}"),
            })
            .collect();
        store.log(1, lines, POSITION, 64 << 20).await;
    }
    store.flush().await;
    let outline = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(outline.lines, LINES);
    assert_eq!((outline.groups.len(), outline.marks.len()), (1000, 1000));
    assert_eq!(outline.warnings, LINES / 4);
    assert!(outline.capped, "past its bounds, it counts");
    let size = serde_json::to_vec(&outline).unwrap().len();
    assert!(
        size < 256 << 10,
        "the outline of 51,200 lines is {size} bytes"
    );

    let search = Search::new(&LogSearchQuery {
        q: "absent".to_owned(),
        ..LogSearchQuery::default()
    })
    .unwrap();
    let page = store.search_log(1, search).await.unwrap();
    let next = page.next.unwrap();
    // Half its lines are about 1 KiB, half a few bytes: 4 MiB is about 8,200 lines.
    assert!(
        (8000..8400).contains(&next),
        "a page scans about 4 MiB, to line {next}"
    );
    let lines = store
        .styled_lines(
            1,
            7,
            LogLinesQuery {
                limit: Some(3),
                ..LogLinesQuery::default()
            }
            .page(),
        )
        .await
        .unwrap();
    assert_eq!(
        lines.iter().map(|line| line.line).collect::<Vec<_>>(),
        [7, 8, 9]
    );
}

/// Reads job 1's outline until `done`, asserting each is of one moment: `of_one_moment(outline)`.
async fn read_while(
    store: &Store,
    done: impl Fn() -> bool,
    of_one_moment: impl Fn(&LogOutline),
) -> u32 {
    let mut reads = 0;
    while !done() {
        of_one_moment(&store.log_outline(1).await.unwrap().unwrap());
        reads += 1;
    }
    reads
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outline_read_while_its_log_grows_is_of_one_moment() {
    let dir = TestDir::new("outline-growing");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    running(&store, 1);
    store.flush().await;
    let writer = tokio::spawn({
        let store = store.clone();
        async move {
            for batch in 0..400 {
                let group = vec![format!("##[group]batch {batch}"), "##[endgroup]".to_owned()];
                store.log(1, group, POSITION, u64::MAX).await;
                store.flush().await;
            }
        }
    });
    let reads = read_while(
        &store,
        || writer.is_finished(),
        |outline| {
            assert_eq!(
                outline.groups.len() as u64 * 2,
                outline.lines,
                "every group stored, and no other: {outline:?}"
            );
            assert!(
                outline
                    .groups
                    .iter()
                    .all(|group| group.end.is_some_and(|end| end < outline.lines)),
                "each group closed in a batch stored with it: {outline:?}"
            );
        },
    )
    .await;
    writer.await.unwrap();
    assert!(reads > 0, "the outline was read while the log grew");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outline_read_while_its_log_is_pruned_is_of_one_moment() {
    let dir = TestDir::new("outline-pruned");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    running(&store, 1);
    // Many short groups in the failed step, each a segment whose first and last lines are read.
    let mut lines = vec![at(2, "##[group]Run make test")];
    for group in 0..900 {
        lines.extend([
            at(2 + group % 7, &format!("##[group]case {group}")),
            at(8, "##[endgroup]"),
        ]);
    }
    lines.push(at(9, "##[error]Process completed with exit code 2."));
    store.log(1, lines, POSITION, u64::MAX).await;
    finish(&store, 1, "failure");
    store.flush().await;
    let whole = store.log_outline(1).await.unwrap().unwrap();
    assert_eq!(whole.evidence, LogEvidence::Complete);
    assert_eq!(
        whole.failure.as_ref().and_then(|failure| failure.exit_code),
        Some(2)
    );

    // Pruning deletes from the first line. Here the first half goes and comes back, again and again, so reads land
    // on either side of many deletions; then the log and its outline go.
    let pruner = tokio::task::spawn_blocking(move || {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TEMP TABLE first_half AS SELECT * FROM log_lines WHERE job = 1 AND line < 900",
        )
        .unwrap();
        for _ in 0..100 {
            db.execute_batch(
                "DELETE FROM log_lines WHERE job = 1 AND line < 900;
                 INSERT INTO log_lines SELECT * FROM first_half;",
            )
            .unwrap();
        }
        db.execute("DELETE FROM log_lines WHERE job = 1", [])
            .unwrap();
        crate::store::outline::delete(&db, 1).unwrap();
    });
    let reads = read_while(
        &store,
        || pruner.is_finished(),
        |outline| {
            let expired = outline.evidence == LogEvidence::Expired
                && outline
                    .steps
                    .iter()
                    .all(|step| !matches!(step.lines, StepLog::Lines(_)))
                && outline
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.exit_code.is_none());
            assert!(
                *outline == whole || expired,
                "an outline is the whole log's or an expired one's: {outline:?}"
            );
        },
    )
    .await;
    pruner.await.unwrap();
    assert!(reads > 0, "the outline was read while the log was pruned");
}

#[tokio::test]
async fn a_log_of_capture_sized_lines_is_read_within_bounds() {
    let dir = TestDir::new("outline-wide");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    running(&store, 1);
    // 47 lines of about 1 MiB, a colour change on every character, between a step's header and its failure.
    let wide = "\u{1b}[31ma\u{1b}[32mb".repeat(1 << 16);
    let mut lines = vec![at(2, "##[group]Run make test")];
    lines.extend(vec![at(3, &wide); 47]);
    lines.push(at(9, "##[error]Process completed with exit code 2."));
    store.log(1, lines, POSITION, 64 << 20).await;
    finish(&store, 1, "failure");
    store.flush().await;

    // At the read boundary: a read takes no more than its budget and one line's prefix.
    let db = Connection::open(&path).unwrap();
    let read = crate::store::read::lines_from(
        &db,
        1,
        LineRead {
            from: 0,
            limit: 500,
            prefix: LINE_BYTES_MAX,
            bytes: 1 << 20,
        },
    )
    .unwrap();
    let bytes: u64 = read.iter().map(|line| line.text.len() as u64).sum();
    assert!(
        bytes < (1 << 20) + LINE_BYTES_MAX,
        "a read of 1 MiB took {bytes}"
    );
    assert_eq!(read[1].omitted, at(3, &wide).len() as u64 - LINE_BYTES_MAX);

    let outline = store.log_outline(1).await.unwrap().unwrap();
    let failure = outline.failure.clone().unwrap();
    assert_eq!(failure.exit_code, Some(2));
    assert!(
        failure
            .excerpt
            .iter()
            .flatten()
            .take(9)
            .all(|line| line.omitted > 0),
        "the excerpt's wide lines say they are cut"
    );
    let size = serde_json::to_vec(&outline).unwrap().len();
    assert!(size < 256 << 10, "the outline is {size} bytes");

    let page = store.styled_lines(1, 0, 1000).await.unwrap();
    assert_eq!(page.len(), 49, "about 47 MiB is stored, 3 MiB is read");
    assert!(
        page.iter().all(|line| line.runs.len() <= RUNS_MAX),
        "no line past its runs"
    );

    let search = Search::new(&LogSearchQuery {
        q: "a".to_owned(),
        limit: Some(200),
        context: Some(5),
        ..LogSearchQuery::default()
    })
    .unwrap();
    let found = store.search_log(1, search).await.unwrap();
    assert_eq!((found.matches.len(), found.next), (48, None));
    let size = serde_json::to_vec(&found).unwrap().len();
    assert!(
        size < 4 << 20,
        "a page of marked wide lines is {size} bytes"
    );
}
