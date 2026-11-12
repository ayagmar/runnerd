//! The open job's log as the keys move through it and lines arrive: the row the keys are on stays, only the lines
//! near it are read, a match is shown where it is, and a running job's newest lines stay in sight.

use runnerd_core::api::{
    FailureSummary, LineRange, LogEvidence, LogGroup, LogOutline, LogSearch, StepLines, StepLog,
    StyledLine,
};

use super::super::log::{LogView, Row};

fn range(from: u64, to: u64) -> LineRange {
    LineRange { from, to }
}

fn lines(from: u64, to: u64) -> Vec<StyledLine> {
    (from..to)
        .map(|line| StyledLine::parse(line, &format!("line {line}")))
        .collect()
}

fn outline(
    evidence: LogEvidence,
    total: u64,
    steps: &[(u32, StepLog)],
    groups: Vec<LogGroup>,
    failed: Option<u32>,
) -> LogOutline {
    LogOutline {
        job: 7,
        evidence,
        lines: total,
        from: 0,
        groups,
        steps: steps
            .iter()
            .map(|(number, lines)| StepLines {
                number: *number,
                lines: *lines,
            })
            .collect(),
        unattributed: Vec::new(),
        errors: 0,
        warnings: 0,
        marks: Vec::new(),
        capped: false,
        failure: failed.map(|step| FailureSummary {
            step: Some(step),
            exit_code: Some(1),
            excerpt: None,
            shell: None,
        }),
    }
}

/// A failed step of `total` lines, its last 5,000 read, as a job's page opens it.
fn failed_step(total: u64) -> LogView {
    let mut view = LogView::default();
    view.take_outline(Ok(outline(
        LogEvidence::Complete,
        total,
        &[(1, StepLog::Lines(range(0, total)))],
        Vec::new(),
        Some(1),
    )));
    view.take_lines(lines(total - 5_000, total));
    view
}

fn on(view: &LogView) -> Row {
    let rows = view.rows();
    rows[view.cursor().expect("the keys are on a row")]
}

#[test]
fn the_row_the_keys_are_on_stays_when_lines_arrive_or_go_above_it() {
    let mut view = failed_step(30_000);
    view.end();
    view.step(-12);
    let before = on(&view);
    assert!(matches!(before, Row::Line(_)), "on a line: {before:?}");
    // The page the outline asked for arrives, adding 1,000 rows above the keys.
    view.take_lines(lines(0, 1_000));
    assert_eq!(on(&view), before, "the same line after a page above it");
    // And lines far below are forgotten as more arrive.
    view.take_lines(lines(1_000, 20_000));
    assert_eq!(
        on(&view),
        before,
        "the same line after lines were forgotten"
    );
}

#[test]
fn moving_through_a_long_step_reads_small_pages_near_the_keys_once_each() {
    let mut view = failed_step(30_000);
    view.start();
    view.step(1);
    assert_eq!(
        on(&view),
        Row::Missing(range(0, 25_000)),
        "on the lines not read yet"
    );
    let mut asked: Vec<LineRange> = Vec::new();
    for _ in 0..30 {
        view.step(100);
        for page in view.wants() {
            assert!(
                page.to - page.from <= 1_000,
                "a page of at most 1,000 lines: {page:?}"
            );
            assert!(
                !asked
                    .iter()
                    .any(|seen| seen.from < page.to && page.from < seen.to),
                "{page:?} asked again: {asked:?}"
            );
            asked.push(page);
            view.take_lines(lines(page.from, page.to));
        }
    }
    let read: u64 = asked.iter().map(|page| page.to - page.from).sum();
    assert!(read <= 5_000, "3,000 rows moved read {read} lines");

    // The lines under the keys stay when a page far from them comes.
    let mut view = failed_step(30_000);
    view.end();
    view.step(-4_000);
    let Row::Line(line) = on(&view) else {
        panic!("on a line");
    };
    view.take_lines(lines(5_000, 20_000));
    // Full, a page far from the keys comes: lines far from them go, not the ones under them nor the page.
    view.take_lines(lines(0, 1_000));
    assert!(
        view.line(line).is_some(),
        "line {line} under the keys is kept"
    );
    assert!(view.line(0).is_some(), "the page just read is kept");
}

#[test]
fn a_match_in_lines_not_read_yet_is_shown_and_the_lines_around_it_read() {
    let mut view = failed_step(60_000);
    view.search("needle".to_owned());
    view.take_found(
        "needle",
        LogSearch {
            matches: vec![45_000],
            lines: vec![StyledLine::parse(45_000, "needle")],
            next: None,
        },
    );
    assert_eq!(on(&view), Row::Line(45_000), "the keys are on the match");
    let wanted = view.wants();
    assert!(
        wanted.iter().any(|page| page.to == 45_000),
        "the page before it: {wanted:?}"
    );
    assert!(
        wanted.iter().any(|page| page.from == 45_001),
        "the page after it: {wanted:?}"
    );
}

#[test]
fn a_followed_job_shows_its_newest_lines_in_open_groups_and_new_steps() {
    let group = |start, end| LogGroup {
        start,
        end,
        depth: 0,
        title: "Compile".to_owned(),
    };
    let mut view = LogView::default();
    view.take_outline(Ok(outline(
        LogEvidence::Running,
        15,
        &[
            (1, StepLog::Lines(range(0, 10))),
            (2, StepLog::Lines(range(10, 15))),
        ],
        vec![group(12, None)],
        None,
    )));
    view.take_lines(lines(0, 15));
    assert!(
        view.rows().contains(&Row::Line(13)),
        "a line of the group still open shows: {:?}",
        view.rows()
    );
    // Step 3 starts: the outline is read again.
    view.take_outline(Ok(outline(
        LogEvidence::Running,
        18,
        &[
            (1, StepLog::Lines(range(0, 10))),
            (2, StepLog::Lines(range(10, 15))),
            (3, StepLog::Lines(range(15, 18))),
        ],
        vec![group(12, Some(14))],
        None,
    )));
    view.take_lines(lines(15, 18));
    assert!(
        view.rows().contains(&Row::Line(16)),
        "the new step's lines show: {:?}",
        view.rows()
    );
}

#[test]
fn a_failed_step_without_lines_of_its_own_opens_the_last_part_with_lines() {
    let mut view = LogView::default();
    view.take_outline(Ok(outline(
        LogEvidence::Truncated,
        20,
        &[
            (1, StepLog::Lines(range(0, 10))),
            (2, StepLog::Lines(range(10, 20))),
            (3, StepLog::Empty),
        ],
        Vec::new(),
        Some(3),
    )));
    assert_eq!(on(&view), Row::Section(1), "the keys are on step 2");
    assert!(view.is_open(Row::Section(1)), "step 2 opens");
}

/// Reads what the log wants until it wants nothing, as `app` does after each key and each page: every page asked is
/// answered at once. Returns the pages asked; fails past `most`, as a log that asks without end would.
fn settle(view: &mut LogView, total: u64, most: usize) -> Vec<LineRange> {
    let mut asked = Vec::new();
    loop {
        let pages = view.wants();
        if pages.is_empty() {
            return asked;
        }
        for page in pages {
            asked.push(page);
            assert!(asked.len() <= most, "the reads do not end: {asked:?}");
            view.take_lines(lines(page.from, page.to.min(total)));
        }
    }
}

#[test]
fn folding_a_long_step_of_a_followed_job_ends_its_reads() {
    let mut view = LogView::default();
    // Step 1 runs, and opens as the step followed.
    view.take_outline(Ok(outline(
        LogEvidence::Running,
        100,
        &[(1, StepLog::Lines(range(0, 100)))],
        Vec::new(),
        None,
    )));
    view.take_lines(lines(0, 100));
    settle(&mut view, 100, 1_000);
    // Step 2 starts.
    view.take_outline(Ok(outline(
        LogEvidence::Running,
        25_100,
        &[
            (1, StepLog::Lines(range(0, 100))),
            (2, StepLog::Lines(range(100, 25_100))),
        ],
        Vec::new(),
        None,
    )));
    // Step 2 prints more than the lines kept, followed in batches.
    for from in (100..25_100).step_by(500) {
        view.take_lines(lines(from, (from + 500).min(25_100)));
        settle(&mut view, 25_100, 1_000);
    }
    // Step 3 starts.
    view.take_outline(Ok(outline(
        LogEvidence::Running,
        25_110,
        &[
            (1, StepLog::Lines(range(0, 100))),
            (2, StepLog::Lines(range(100, 25_100))),
            (3, StepLog::Lines(range(25_100, 25_110))),
        ],
        Vec::new(),
        None,
    )));
    view.take_lines(lines(25_100, 25_110));
    settle(&mut view, 25_110, 1_000);

    // The keys go to step 2's header and fold it.
    let header = view
        .rows()
        .iter()
        .position(|row| *row == Row::Section(1))
        .expect("step 2 has a header");
    view.point(header);
    view.toggle();
    assert!(!view.is_open(Row::Section(1)), "step 2 is folded");
    assert!(view.is_open(Row::Section(0)), "step 1 stays open");
    let asked = settle(&mut view, 25_110, 50);
    for (at, page) in asked.iter().enumerate() {
        assert!(
            !asked[..at].contains(page),
            "{page:?} is read twice: {asked:?}"
        );
    }
    assert!(
        view.rows().contains(&Row::Line(25_105)),
        "step 3's lines stay read: {:?}",
        view.rows()
    );
}
