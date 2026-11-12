use super::outline::{DEPTH_MAX, GROUP_MAX, MARK_MAX};
use super::search::{CONTEXT_MAX, SCAN_BYTES, SCAN_LINES};
use super::*;
use crate::api::{
    Job, JobMetrics, JobState, JobSummary, LogSummary, RecordNote, RecordRemoval, Step, Timeline,
};

/// 2026-10-23T01:00:00Z.
const HOUR: u64 = 1_792_717_200;

/// A line the runner wrote at `mm:ss.f` past [`HOUR`].
fn at(time: &str, text: &str) -> String {
    format!("2026-10-23T01:{time}Z {text}")
}

fn step(number: u32, name: &str, times: Option<(u64, u64)>, conclusion: &str) -> Step {
    Step {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: times.map(|(start, _)| HOUR + start),
        completed_at: times.map(|(_, end)| HOUR + end),
    }
}

fn job(state: JobState, conclusion: &str, steps: Vec<Step>) -> Job {
    Job {
        summary: JobSummary {
            id: 7,
            repository: "halyard/ledger".to_owned(),
            run_id: 1,
            run_attempt: 1,
            workflow: Some("CI".to_owned()),
            name: "build".to_owned(),
            branch: None,
            url: None,
            labels: vec![],
            state,
            conclusion: Some(conclusion.to_owned()),
            runner: None,
            timeline: Timeline::default(),
            metrics: JobMetrics::default(),
            log: LogSummary::default(),
            runner_fault: false,
        },
        steps,
        record: None,
        admission: None,
    }
}

/// Set-up groups, a checkout, a step with a custom name whose header shares a second with the checkout's end and
/// whose output holds nested groups, a group without timestamps and a user group titled as the next steps are; two
/// steps with one name, the second failing with exit code 2; a skipped step; a post step and `Complete job`.
fn steps() -> Vec<Step> {
    vec![
        step(1, "Set up job", Some((0, 2)), "success"),
        step(2, "Run actions/checkout@v4", Some((2, 4)), "success"),
        step(3, "Build", Some((4, 30)), "success"),
        step(4, "Run make", Some((30, 40)), "success"),
        step(5, "Run make", Some((41, 50)), "failure"),
        step(6, "Lint", None, "skipped"),
        step(7, "Post Run actions/checkout@v4", Some((51, 52)), "success"),
        step(8, "Complete job", Some((53, 53)), "success"),
    ]
}

fn log() -> Vec<String> {
    vec![
        at("00:00.1", "Current runner version: '2.330.0'"),
        at("00:00.2", "##[group]Operating System"),
        at("00:00.2", "Ubuntu"),
        at("00:00.3", "##[endgroup]"),
        at("00:01.0", "##[group]GITHUB_TOKEN Permissions"),
        at("00:01.0", "Contents: read"),
        at("00:01.1", "##[endgroup]"),
        at("00:02.1", "##[group]Run actions/checkout@v4"),
        at("00:02.1", "with:"),
        at("00:02.1", "##[endgroup]"),
        at("00:02.5", "##[command]/usr/bin/git version"),
        at("00:03.9", "Syncing repository"),
        // 12: the custom-named step's header, in the second the checkout ended.
        at("00:04.2", "##[group]Run make build"),
        at("00:04.2", "\u{1b}[36;1mmake build\u{1b}[0m"),
        at("00:04.2", "##[endgroup]"),
        at("00:05.0", "##[group]Compile"),
        at("00:06.0", "##[group]Run tests in parallel"),
        at("00:20.0", "ok"),
        at("00:21.0", "##[endgroup]"),
        at("00:22.0", "##[endgroup]"),
        // 20: no timestamps.
        "##[group]Artifacts".to_owned(),
        "uploaded".to_owned(),
        "##[endgroup]".to_owned(),
        // 23: a user group titled as steps 4 and 5 are.
        at("00:24.0", "##[group]Run make"),
        at("00:25.0", "##[endgroup]"),
        at("00:29.0", "##[warning]deprecated flag"),
        // 26: step 4's header, in the second step 3 ended.
        at("00:30.3", "##[group]Run make"),
        at("00:30.3", "##[endgroup]"),
        at("00:31.0", "##[group]Cache"),
        at("00:31.5", "##[endgroup]"),
        at("00:39.0", "built"),
        // 31: step 5.
        at("00:41.0", "##[group]Run make"),
        at("00:41.0", "##[endgroup]"),
        at("00:45.0", "\u{1b}[31merror: missing target\u{1b}[0m"),
        at("00:49.9", "##[error]Process completed with exit code 2."),
        // 35: the post step, then `Complete job`.
        at("00:51.0", "Post job cleanup."),
        at("00:51.5", "##[command]/usr/bin/git version"),
        at("00:53.1", "Cleaning up orphan processes"),
    ]
}

fn lines(from: u64, to: u64) -> StepLog {
    StepLog::Lines(LineRange { from, to })
}

#[test]
fn a_styled_line_has_its_time_and_command_split_off_and_only_closed_styles() {
    let line = StyledLine::parse(
        4,
        "2026-10-23T01:00:45.1234567Z \u{1b}[1;31merror\u{1b}[0m: \u{1b}[38;5;10mok\u{1b}[39m \
         \u{1b}[38;2;1;2;3mrgb\u{1b}[m\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7}\u{1b}[2K\r\tdone\u{7}",
    );
    assert_eq!(line.at, Some(HOUR + 45));
    assert_eq!(line.kind, LineKind::Output);
    let run = |text: &str, color, bold| Run {
        text: text.to_owned(),
        color,
        bold,
        mark: false,
    };
    assert_eq!(
        line.runs,
        [
            run("error", Some(Color::Red), true),
            run(": ", None, false),
            run("ok", Some(Color::Green), false),
            run(" rgblink\tdone", None, false),
        ]
    );
    let json = serde_json::to_string(&line).unwrap();
    assert!(
        !json.contains("\\u001b") && !json.contains("\\u0007") && !json.contains("\\r"),
        "no escape or control character but a tab leaves core: {json}"
    );

    let error = StyledLine::parse(
        9,
        &at("00:49.9", "##[error]Process completed with exit code 2."),
    );
    assert_eq!(
        (error.kind, error.text().as_str()),
        (LineKind::Error, "Process completed with exit code 2.")
    );
    let group = StyledLine::parse(1, "##[group]Run \u{1b}[1mmake\u{1b}[0m");
    assert_eq!(
        (group.at, group.kind, group.text().as_str()),
        (None, LineKind::Group, "Run make")
    );
    let unknown = StyledLine::parse(2, &at("00:00.0", "##[sparkle]shiny"));
    assert_eq!(
        (unknown.kind, unknown.text().as_str()),
        (LineKind::Output, "##[sparkle]shiny"),
        "a command the runner does not name stays as written"
    );
    let replaced = StyledLine::parse(3, &String::from_utf8_lossy(b"bad\xe9 \xff"));
    assert_eq!(replaced.text(), "bad\u{fffd} \u{fffd}");
}

#[test]
fn steps_get_their_lines_by_time_never_by_title_or_position() {
    let job = job(JobState::Completed, "failure", steps());
    let outline = LogOutline::of(&job, &log());
    assert_eq!(outline.evidence, LogEvidence::Complete);
    let found: Vec<(u32, StepLog)> = outline
        .steps
        .iter()
        .map(|step| (step.number, step.lines))
        .collect();
    assert_eq!(
        found,
        [
            (1, lines(0, 7)),
            (2, lines(7, 12)),
            // Its header shares a second with the checkout and has no title to tell them apart; the group without
            // timestamps between its own groups is its, and so is the user group titled `Run make`, by time.
            (3, lines(15, 26)),
            // Its header shares a second with step 3: its title, step 4's name, could be any step's output.
            (4, lines(28, 31)),
            (5, lines(31, 35)),
            (6, StepLog::Empty),
            (7, lines(35, 37)),
            (8, lines(37, 38)),
        ]
    );
    assert_eq!(
        outline.unattributed,
        [
            LineRange { from: 12, to: 15 },
            LineRange { from: 26, to: 28 }
        ]
    );
    let groups: Vec<(u64, Option<u64>, u32, &str)> = outline
        .groups
        .iter()
        .map(|group| (group.start, group.end, group.depth, group.title.as_str()))
        .collect();
    assert_eq!(
        groups[4..7],
        [
            (15, Some(19), 0, "Compile"),
            (16, Some(18), 1, "Run tests in parallel"),
            (20, Some(22), 0, "Artifacts"),
        ]
    );
    assert_eq!(
        outline.marks,
        [
            LogMark {
                line: 25,
                kind: MarkKind::Warning
            },
            LogMark {
                line: 34,
                kind: MarkKind::Error
            },
        ]
    );
    assert_eq!(
        (outline.errors, outline.warnings, outline.capped),
        (1, 1, false)
    );

    // Shuffled step numbers or a missing step do not move lines by position.
    let mut fewer = job.clone();
    fewer.steps.retain(|step| step.number != 2);
    let outline = LogOutline::of(&fewer, &log());
    assert_eq!(
        outline.step_lines(3),
        Some(LineRange { from: 12, to: 26 }),
        "with the checkout gone, the header is step 3's alone"
    );
    assert_eq!(outline.step_lines(1), Some(LineRange { from: 0, to: 7 }));
}

#[test]
fn a_step_whose_lines_cannot_be_told_apart_is_unattributed_not_guessed() {
    // Two steps in one second, and a log with no timestamps at all.
    let steps = vec![
        step(1, "Set up job", Some((0, 1)), "success"),
        step(2, "Unit tests", Some((1, 1)), "success"),
        step(3, "Integration", Some((1, 1)), "failure"),
    ];
    let log: Vec<String> = [
        "##[group]Run cargo test",
        "ok",
        "##[group]Run cargo test --ignored",
        "##[error]Process completed with exit code 101.",
    ]
    .map(str::to_owned)
    .to_vec();
    let outline = LogOutline::of(&job(JobState::Completed, "failure", steps), &log);
    assert!(
        outline
            .steps
            .iter()
            .all(|step| step.lines == StepLog::Unattributed),
        "{:?}",
        outline.steps
    );
    assert_eq!(outline.unattributed, [LineRange { from: 0, to: 4 }]);
    let failure = outline.failure.unwrap();
    assert_eq!(
        (failure.step, failure.exit_code, failure.excerpt),
        (Some(3), None, None),
        "the exit code and the last lines are the step's only where its lines are known"
    );
}

#[test]
fn groups_stay_balanced_past_their_bounds_and_unclosed_ones_stay_open() {
    let mut log: Vec<String> = (0..DEPTH_MAX + 2)
        .map(|depth| format!("##[group]level {depth}"))
        .collect();
    // The two past the depth close first.
    log.extend(vec!["##[endgroup]".to_owned(); 3]);
    let job = job(JobState::Running, "", Vec::new());
    let outline = LogOutline::of(&job, &log);
    assert_eq!(outline.groups.len(), DEPTH_MAX);
    assert!(
        outline.capped,
        "groups past the depth are counted, not kept"
    );
    let ends: Vec<Option<u64>> = outline.groups.iter().map(|group| group.end).collect();
    let mut expected = vec![None; DEPTH_MAX];
    expected[DEPTH_MAX - 1] = Some(12);
    assert_eq!(
        ends, expected,
        "the deepest kept group closes at the third end"
    );
    assert_eq!(outline.evidence, LogEvidence::Running);

    let many: Vec<String> = (0..GROUP_MAX + 5)
        .flat_map(|_| {
            [
                "##[group]g".to_owned(),
                "##[endgroup]".to_owned(),
                "##[warning]w".to_owned(),
            ]
        })
        .chain((0..MARK_MAX).map(|_| "##[error]e".to_owned()))
        .collect();
    let outline = LogOutline::of(&job, &many);
    assert_eq!(outline.groups.len() as u64, GROUP_MAX);
    assert_eq!(outline.marks.len() as u64, MARK_MAX);
    assert_eq!(
        (outline.warnings, outline.errors),
        (GROUP_MAX + 5, MARK_MAX)
    );
    assert!(outline.capped, "counts go on past what is kept");
}

#[test]
fn a_failure_summary_says_each_part_only_on_its_evidence() {
    let mut failed = job(JobState::Completed, "failure", steps());
    let outline = LogOutline::of(&failed, &log());
    let failure = outline.failure.clone().unwrap();
    assert_eq!((failure.step, failure.exit_code), (Some(5), Some(2)));
    let excerpt: Vec<u64> = failure
        .excerpt
        .iter()
        .flatten()
        .map(|line| line.line)
        .collect();
    assert_eq!(excerpt, [31, 32, 33, 34]);
    assert_eq!(failure.shell, None, "no record");
    assert_eq!(
        failure.headline(&failed.steps),
        "Step 5, Run make, failed with exit code 2"
    );

    failed.record = Some(RecordNote {
        created_at: Some(HOUR),
        bytes: Some(1 << 30),
        skipped: None,
        removed_at: None,
        removed_by: None,
    });
    let kept = LogOutline::of(&failed, &log()).failure.unwrap();
    assert_eq!(kept.shell.as_deref(), Some("runnerctl shell 7"));
    failed.record = failed.record.map(|record| RecordNote {
        removed_at: Some(HOUR + 60),
        removed_by: Some(RecordRemoval::Age),
        ..record
    });
    assert_eq!(
        LogOutline::of(&failed, &log()).failure.unwrap().shell,
        None,
        "a removed record opens no shell"
    );

    // Cut at the cap before the failed step: it has no lines here, so nothing of it is known.
    let mut cut = failed.clone();
    cut.summary.log.truncated = true;
    let outline = LogOutline::of(&cut, &log()[..20]);
    assert_eq!(outline.evidence, LogEvidence::Truncated);
    let failure = outline.failure.unwrap();
    assert_eq!(
        (failure.step, failure.exit_code, failure.excerpt),
        (Some(5), None, None)
    );

    let mut lost = job(
        JobState::Lost,
        "",
        vec![step(1, "Set up job", Some((0, 2)), "success")],
    );
    lost.summary.conclusion = None;
    let failure = LogOutline::of(&lost, &[]).failure.unwrap();
    assert_eq!(failure.step, None);
    assert_eq!(failure.headline(&lost.steps), "No step reports the failure");
    assert_eq!(
        LogOutline::of(&job(JobState::Completed, "success", steps()), &log()).failure,
        None
    );
}

#[test]
fn evidence_is_read_in_order_and_worded_by_core() {
    let finished = job(JobState::Completed, "success", Vec::new());
    let read = |job: &Job, stored: StoredLog, saved: Option<SavedOutline>| {
        LogOutline::assemble(
            job,
            stored,
            OutlineParts {
                saved,
                ..OutlineParts::default()
            },
            &mut |_| Vec::new(),
        )
        .evidence
    };
    let stored = StoredLog {
        lines: 10,
        truncated: false,
        kept: true,
    };
    let outlined = Some(SavedOutline::default());
    assert_eq!(
        read(&finished, stored, outlined.clone()),
        LogEvidence::Complete
    );
    assert_eq!(
        read(&finished, StoredLog { lines: 0, ..stored }, None),
        LogEvidence::Missing
    );
    assert_eq!(
        read(
            &finished,
            StoredLog {
                kept: false,
                ..stored
            },
            outlined.clone()
        ),
        LogEvidence::Expired
    );
    assert_eq!(read(&finished, stored, None), LogEvidence::NotOutlined);
    assert_eq!(
        read(
            &finished,
            StoredLog {
                truncated: true,
                ..stored
            },
            outlined.clone()
        ),
        LogEvidence::Truncated
    );
    assert_eq!(
        read(
            &finished,
            stored,
            Some(SavedOutline {
                from: 4,
                ..SavedOutline::default()
            })
        ),
        LogEvidence::Partial
    );
    let running = job(JobState::Running, "", Vec::new());
    assert_eq!(
        read(&running, StoredLog { lines: 0, ..stored }, None),
        LogEvidence::Running
    );
    // The cap reached before the first line was stored, while running and after.
    let capped = StoredLog {
        lines: 0,
        truncated: true,
        kept: false,
    };
    assert_eq!(read(&finished, capped, None), LogEvidence::Truncated);
    assert_eq!(read(&running, capped, None), LogEvidence::Truncated);
    assert_eq!(
        read(
            &finished,
            StoredLog {
                truncated: true,
                ..stored
            },
            None
        ),
        LogEvidence::Truncated,
        "a cut log is worth a look, outlined or not"
    );
    assert_eq!(read(&running, stored, outlined), LogEvidence::Running);

    assert_eq!(LogEvidence::Complete.words(), None);
    assert_eq!(
        LogEvidence::Truncated.words(),
        Some("cut at history.log_max")
    );
    assert_eq!(LogEvidence::Truncated.tone(), crate::api::Tone::Warn);
    let future: LogEvidence = serde_json::from_str("\"rewritten\"").unwrap();
    assert_eq!(
        future,
        LogEvidence::Unknown,
        "a newer daemon's reading is not taken for complete"
    );
}

#[test]
fn a_newer_daemons_kinds_read_as_unknown() {
    let line: StyledLine = serde_json::from_str(
        r#"{"line":1,"kind":"sparkle","runs":[{"text":"x","color":"chartreuse","glow":true}]}"#,
    )
    .unwrap();
    assert_eq!(
        (line.kind, line.runs[0].color),
        (LineKind::Unknown, Some(Color::Unknown))
    );
    let step: StepLines =
        serde_json::from_str(r#"{"number":2,"lines":{"kind":"guessed","from":1}}"#).unwrap();
    assert_eq!(step.lines, StepLog::Unknown);
    let mark: LogMark = serde_json::from_str(r#"{"line":3,"kind":"notice"}"#).unwrap();
    assert_eq!(mark.kind, MarkKind::Unknown);
}

/// Searches `log` for `query` as the store does, a batch at a time.
fn search(log: &[String], query: &LogSearchQuery) -> LogSearch {
    let mut search = Search::new(query).unwrap();
    while let Some(read) = search.wants() {
        search.read(read.of(log));
    }
    search.finish()
}

#[test]
fn a_search_marks_its_matches_with_their_context_and_pages_on() {
    let query =
        |q: &str, limit: Option<u32>, context: Option<u32>, from: Option<u64>| LogSearchQuery {
            q: q.to_owned(),
            from,
            limit,
            context,
            follow: false,
        };
    let found = search(&log(), &query("MAKE", None, Some(1), None));
    assert_eq!(found.matches, [12, 13, 23, 26, 31]);
    assert_eq!(found.next, None, "every stored line was scanned");
    let shown: Vec<u64> = found.lines.iter().map(|line| line.line).collect();
    assert_eq!(
        shown,
        [11, 12, 13, 14, 22, 23, 24, 25, 26, 27, 30, 31, 32],
        "context once each, in order"
    );
    let line_13 = found.lines.iter().find(|line| line.line == 13).unwrap();
    assert_eq!(
        line_13.runs,
        [
            Run {
                text: "make".to_owned(),
                color: Some(Color::Cyan),
                bold: true,
                mark: true
            },
            Run {
                text: " build".to_owned(),
                color: Some(Color::Cyan),
                bold: true,
                mark: false
            },
        ],
        "a match keeps the style it was printed in"
    );

    let first = search(&log(), &query("make", Some(2), Some(2), None));
    assert_eq!(
        (first.matches.as_slice(), first.next),
        ([12, 13].as_slice(), Some(14))
    );
    assert_eq!(
        first.lines.last().map(|line| line.line),
        Some(15),
        "the last match keeps its context after the page ends"
    );
    let second = search(&log(), &query("make", Some(2), None, first.next));
    assert_eq!(second.matches, [23, 26]);

    let accented = vec![at("00:00.0", "Café ÉTÉ")];
    assert_eq!(
        search(&accented, &query("café", None, None, None)).matches,
        [0]
    );
    assert!(
        search(&accented, &query("été", None, None, None))
            .matches
            .is_empty(),
        "only ASCII letters match in either case"
    );
    let across = vec![at("00:00.0", "\u{1b}[31mer\u{1b}[0mror")];
    let found = search(&across, &query("error", None, None, None));
    assert_eq!(
        found.lines[0]
            .runs
            .iter()
            .filter(|run| run.mark)
            .map(|run| run.text.as_str())
            .collect::<String>(),
        "error"
    );

    for refused in [
        query("", None, None, None),
        query(&"x".repeat(SEARCH_QUERY_MAX + 1), None, None, None),
        LogSearchQuery {
            follow: true,
            ..query("x", None, None, None)
        },
    ] {
        assert!(Search::new(&refused).is_err(), "{refused:?}");
    }
}

#[test]
fn a_search_reads_no_more_than_its_budget_per_page() {
    let log: Vec<String> = (0..SCAN_LINES + 10)
        .map(|line| format!("line {line}"))
        .collect();
    let found = search(
        &log,
        &LogSearchQuery {
            q: "absent".to_owned(),
            ..LogSearchQuery::default()
        },
    );
    assert_eq!((found.matches.len(), found.next), (0, Some(SCAN_LINES)));
    let rest = search(
        &log,
        &LogSearchQuery {
            q: "line".to_owned(),
            from: found.next,
            ..LogSearchQuery::default()
        },
    );
    assert_eq!(rest.matches.first(), Some(&SCAN_LINES));
    // A line is read to its first 64 KiB: 4 MiB is 64 of them.
    let wide = vec!["y".repeat(1 << 20); 100];
    let found = search(
        &wide,
        &LogSearchQuery {
            q: "absent".to_owned(),
            ..LogSearchQuery::default()
        },
    );
    assert_eq!(found.next, Some(64), "4 MiB a page");
}

#[test]
fn a_title_or_an_untimed_group_never_gives_a_step_lines() {
    // Build ends in second 10, and `Run tests` starts and ends in it. In that second Build prints a group titled as
    // the next step, with an exit code of its own; then the next step's header and its failure follow.
    let steps = vec![
        step(1, "Build", Some((0, 10)), "success"),
        step(2, "Run tests", Some((10, 10)), "failure"),
    ];
    let log = vec![
        at("00:00.5", "##[group]Run make"),
        at("00:05.0", "compiling"),
        at("00:10.1", "##[group]Run tests"),
        at("00:10.1", "##[error]Process completed with exit code 99."),
        at("00:10.2", "##[endgroup]"),
        at("00:10.3", "##[endgroup]"),
        at("00:10.5", "##[group]Run tests"),
        at("00:10.5", "##[endgroup]"),
        at("00:10.9", "##[error]Process completed with exit code 1."),
    ];
    let failed = job(JobState::Completed, "failure", steps);
    let outline = LogOutline::of(&failed, &log);
    assert_eq!(outline.step_lines(1), Some(LineRange { from: 0, to: 2 }));
    assert_eq!(outline.steps[1].lines, StepLog::Unattributed);
    let failure = outline.failure.unwrap();
    assert_eq!(
        (failure.exit_code, failure.excerpt.as_ref()),
        (None, None),
        "neither exit code in that second is known to be the failed step's"
    );
    assert_eq!(failure.headline(&failed.steps), "Step 2, Run tests, failed");

    // A group with no timestamps, titled as the next step, before that step's own lines.
    let steps = vec![
        step(1, "Build", Some((0, 5)), "success"),
        step(2, "Test", Some((6, 9)), "failure"),
    ];
    let log = vec![
        at("00:00.5", "##[group]Run make"),
        at("00:04.0", "built"),
        "##[group]Test".to_owned(),
        "##[error]Process completed with exit code 7.".to_owned(),
        "##[endgroup]".to_owned(),
        at("00:06.5", "##[group]Run pytest"),
        at("00:08.0", "##[error]3 tests failed"),
    ];
    let outline = LogOutline::of(&job(JobState::Completed, "failure", steps), &log);
    assert_eq!(outline.step_lines(2), Some(LineRange { from: 5, to: 7 }));
    assert_eq!(outline.unattributed, [LineRange { from: 2, to: 5 }]);
    assert_eq!(
        outline.failure.unwrap().exit_code,
        None,
        "the untimed group's exit code is no step's"
    );
}

#[test]
fn marking_dense_matches_or_styles_stays_within_a_lines_runs() {
    let needle = LogNeedle::new("A").unwrap();
    let line_max = usize::try_from(LINE_BYTES_MAX).unwrap();

    // Every byte matches: the matches are one marked run.
    let mut dense = StyledLine::parse(0, &"a".repeat(line_max));
    assert!(needle.mark(&mut dense), "a line of `a` holds `A`");
    assert_eq!(dense.runs.len(), 1);
    assert!(dense.runs[0].mark, "the whole line is the match");
    assert_eq!(dense.omitted, 0);

    // Every other byte matches: runs stop at their cap and the rest is counted, not dropped unsaid.
    let mut alternating = StyledLine::parse(1, &"ab".repeat(line_max / 2));
    assert!(needle.mark(&mut alternating), "`ab` holds `A`");
    assert_eq!(alternating.runs.len(), RUNS_MAX);
    assert_eq!(
        alternating.text().len() as u64 + alternating.omitted,
        LINE_BYTES_MAX
    );
    let size = serde_json::to_vec(&alternating).unwrap().len();
    assert!(size < 64 << 10, "a marked line is {size} bytes");

    // A colour change on every character, past the line's cap: runs and bytes stop at theirs.
    let styled = "\u{1b}[31ma\u{1b}[32mb".repeat(1 << 16);
    let mut colourful = StyledLine::parse(2, &styled);
    assert_eq!(colourful.runs.len(), RUNS_MAX);
    assert!(
        colourful.omitted > styled.len() as u64 - LINE_BYTES_MAX,
        "past its runs and its bytes, the line says what it leaves out"
    );
    assert!(needle.mark(&mut colourful), "its `a`s hold `A`");
    assert_eq!(colourful.runs.len(), RUNS_MAX);
}

#[test]
fn a_search_and_a_failure_read_within_their_bytes_whatever_a_lines_size() {
    // Lines as long as the capture keeps, each holding the query.
    let wide = vec![format!("needle {}", "y".repeat(1 << 20)); 100];
    let mut search = Search::new(&LogSearchQuery {
        q: "needle".to_owned(),
        limit: Some(200),
        context: Some(5),
        ..LogSearchQuery::default()
    })
    .unwrap();
    let mut handed = 0;
    while let Some(read) = search.wants() {
        let lines = read.of(&wide);
        handed += lines.iter().map(|line| line.text.len() as u64).sum::<u64>();
        search.read(lines);
    }
    let page = search.finish();
    assert!(
        handed <= SCAN_BYTES + u64::from(CONTEXT_MAX) * LINE_BYTES_MAX,
        "a page read {handed} bytes: its budget, then the last match's context"
    );
    assert_eq!(
        (page.matches.len(), page.next),
        (64, Some(64)),
        "64 lines are read to 64 KiB each"
    );
    assert!(
        page.lines
            .iter()
            .all(|line| line.omitted == wide[0].len() as u64 - LINE_BYTES_MAX),
        "each line says how much of it is not shown"
    );
    let size = serde_json::to_vec(&page).unwrap().len() as u64;
    assert!(size < SCAN_BYTES + (1 << 20), "a page is {size} bytes");

    // A read never splits a character; what it leaves is counted.
    let accented = vec!["é".repeat(10)];
    let cut = LineRead {
        from: 0,
        limit: 1,
        prefix: 5,
        bytes: u64::MAX,
    }
    .of(&accented);
    assert_eq!((cut[0].text.as_str(), cut[0].omitted), ("éé", 16));

    // A failed step whose lines are as long, styled on every character, and a group titled with a whole one.
    let steps = vec![step(1, "Run make", Some((0, 10)), "failure")];
    let mut log = vec![
        at("00:01.0", "##[group]Run make"),
        at("00:01.0", &format!("##[group]{}", "t".repeat(1 << 20))),
        at("00:01.0", "##[endgroup]"),
    ];
    log.extend(vec![
        at("00:02.0", &"\u{1b}[31ma\u{1b}[32mb".repeat(1 << 17));
        20
    ]);
    log.push(at(
        "00:09.0",
        "##[error]Process completed with exit code 2.",
    ));
    let outline = LogOutline::of(&job(JobState::Completed, "failure", steps), &log);
    assert!(outline.groups.iter().all(|group| group.title.len() <= 200));
    let failure = outline.failure.clone().unwrap();
    assert_eq!(failure.exit_code, Some(2));
    let excerpt = failure.excerpt.unwrap();
    assert_eq!(excerpt.len(), 10);
    assert!(
        excerpt[..9]
            .iter()
            .all(|line| line.omitted > 0 && line.text().len() <= 4 << 10),
        "an excerpt shows the start of each line and says it is cut"
    );
    let size = serde_json::to_vec(&outline).unwrap().len();
    assert!(size < 256 << 10, "the outline is {size} bytes");
}

#[test]
fn a_step_name_reaches_the_headline_without_its_escapes() {
    let name = "Run \u{1b}]52;c;ZXZpbA==\u{7}make\r\u{1b}[2J";
    let failed = job(
        JobState::Completed,
        "failure",
        vec![step(1, name, Some((0, 1)), "failure")],
    );
    let failure = LogOutline::of(&failed, &[]).failure.unwrap();
    assert_eq!(failure.headline(&failed.steps), "Step 1, Run make, failed");
}

#[test]
fn a_line_longer_than_the_exit_scan_reads_is_no_exit_code() {
    // The runner's sentence, then carriage returns up to the scan's bound, then more text on the same line: read
    // whole it is a quotation, and its first 256 bytes alone would read as the exit message.
    let sentence = at("00:01.5", "##[error]Process completed with exit code 99.");
    let quoted = format!(
        "{sentence}{} This is a quotation, not the runner's exit message.",
        "\r".repeat(256 - sentence.len())
    );
    let steps = vec![step(1, "Test", Some((0, 2)), "failure")];
    let log = vec![at("00:00.5", "##[group]Run make test"), quoted];
    let outline = LogOutline::of(&job(JobState::Completed, "failure", steps), &log);
    assert_eq!(outline.step_lines(1), Some(LineRange { from: 0, to: 2 }));
    assert_eq!(
        outline.failure.unwrap().exit_code,
        None,
        "a line cut at the scan's bound is not the runner's exit message"
    );
}
