use super::*;
use crate::api::{JobMetrics, JobRunner, LogSummary, Timeline};

fn job(id: u64, queued_at: u64) -> JobSummary {
    JobSummary {
        id,
        repository: "halyard/ledger".to_owned(),
        run_id: 1,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: format!("job {id}"),
        branch: Some("main".to_owned()),
        url: None,
        labels: vec![],
        state: JobState::Queued,
        conclusion: None,
        runner: None,
        timeline: Timeline {
            queued_at,
            ..Timeline::default()
        },
        metrics: JobMetrics::default(),
        log: LogSummary::default(),
        runner_fault: false,
        record_mark: None,
    }
}

/// Job `id`, which runner `r<id>` in `profile`'s `slot` ran from `started` to `finished`.
fn ran(id: u64, profile: &str, slot: usize, started: u64, finished: Option<u64>) -> JobSummary {
    let mut job = job(id, started.saturating_sub(5));
    job.runner = Some(JobRunner {
        name: format!("r{id}"),
        profile: profile.to_owned(),
        slot,
        memory_cap: None,
        cpu_cap: None,
    });
    job.timeline.container_started_at = Some(started.saturating_sub(5));
    job.timeline.started_at = Some(started);
    job.timeline.finished_at = finished;
    job.state = if finished.is_some() {
        JobState::Completed
    } else {
        JobState::Running
    };
    job.conclusion = finished.map(|_| "success".to_owned());
    job.timeline.cleaned_at = finished.map(|at| at + 2);
    job
}

const WINDOW: Span = Span {
    from: 1000,
    to: 2000,
};

fn lane<'timeline>(
    timeline: &'timeline SlotTimeline,
    profile: &str,
    slot: usize,
) -> &'timeline Lane {
    timeline
        .lanes
        .iter()
        .find(|lane| lane.profile == profile && lane.slot == slot)
        .unwrap()
}

#[test]
fn bars_keep_their_real_ends_in_the_slot_their_runner_had_and_jobs_without_one_wait_apart() {
    let mut lost = ran(4, "jvm", 2, 1500, None);
    lost.state = JobState::Lost;
    let mut lost_before = ran(5, "jvm", 2, 500, None);
    lost_before.state = JobState::Lost;
    let mut elsewhere = job(7, 1200);
    elsewhere.state = JobState::Elsewhere;
    elsewhere.timeline.finished_at = Some(1300);
    let jobs = [
        // Across the window's start, and across its end.
        ran(1, "jvm", 1, 900, Some(1100)),
        ran(2, "jvm", 1, 1900, Some(2500)),
        // In a slot the config may no longer have, of a profile it may no longer have.
        ran(3, "retired", 3, 1200, None),
        lost,
        lost_before,
        // Before the window, and after it.
        ran(8, "jvm", 1, 100, Some(200)),
        job(9, 2100),
        job(6, 1400),
        elsewhere,
    ];
    let timeline = SlotTimeline::of(&jobs, WINDOW, vec![WINDOW], 0);
    let bars = |lane: &Lane| lane.bars.iter().map(|bar| bar.job).collect::<Vec<_>>();
    let first = lane(&timeline, "jvm", 1);
    assert_eq!(bars(first), [1, 2]);
    assert_eq!(
        (first.bars[0].started_at, first.bars[1].end),
        (Some(900), BarEnd::At { at: 2500 }),
        "a bar's ends are its own, not the window's"
    );
    assert_eq!(bars(lane(&timeline, "retired", 3)), [3]);
    assert_eq!(
        lane(&timeline, "retired", 3).bars[0].end_words(),
        Some("running")
    );
    let second = lane(&timeline, "jvm", 2);
    assert_eq!(
        bars(second),
        [4],
        "an unknown end shows only from a start in the window"
    );
    assert_eq!(second.bars[0].end_words(), Some("end unknown"));
    assert_eq!(
        timeline
            .unassigned
            .iter()
            .map(|bar| (bar.job, bar.end_words()))
            .collect::<Vec<_>>(),
        [(7, None), (6, Some("waiting"))],
        "by when they were queued"
    );
    assert_eq!(timeline.unassigned[0].outcome().label, "elsewhere");
    assert_eq!(timeline.cut_before, None);
}

#[test]
fn a_lanes_empty_stretch_is_no_job_only_where_runnerd_was_watching() {
    let mut unknown_end = ran(2, "jvm", 1, 1800, None);
    unknown_end.state = JobState::Lost;
    let jobs = [ran(1, "jvm", 1, 1100, Some(1200)), unknown_end];
    let observed = vec![
        Span {
            from: 1000,
            to: 1400,
        },
        // A restart: nothing watched from 1400 to 1600.
        Span {
            from: 1600,
            to: 2000,
        },
    ];
    let timeline = SlotTimeline::of(&jobs, WINDOW, observed, 0);
    let gaps: Vec<(u64, u64, &str)> = lane(&timeline, "jvm", 1)
        .gaps(&timeline)
        .into_iter()
        .map(|gap| (gap.span.from, gap.span.to, gap.kind.words()))
        .collect();
    assert_eq!(
        gaps,
        [
            (1000, 1100, "no job"),
            (1200, 1400, "no job"),
            (1400, 1600, "unknown"),
            (1600, 1800, "no job"),
            (1800, 2000, "unknown"),
        ]
    );
    let blind = SlotTimeline::of(&jobs, WINDOW, Vec::new(), 0);
    assert!(
        lane(&blind, "jvm", 1)
            .gaps(&blind)
            .iter()
            .all(|gap| gap.kind == GapKind::Unknown),
        "without samples nothing reads as free"
    );
}

#[test]
fn past_the_cap_the_latest_bars_are_kept_and_the_cut_is_said() {
    let count = u64::try_from(TIMELINE_BARS_MAX).unwrap() + 1;
    let jobs: Vec<JobSummary> = (0..count).map(|id| job(id + 1, 1000 + id / 4)).collect();
    let timeline = SlotTimeline::of(&jobs, WINDOW, vec![WINDOW], 0);
    assert_eq!(timeline.unassigned.len(), TIMELINE_BARS_MAX);
    assert_eq!(timeline.unassigned[0].job, 2, "the oldest is left out");
    assert_eq!(timeline.cut_before, Some(1000));
}

#[test]
fn names_reach_the_timeline_plain_and_cut() {
    let mut named = job(1, 1500);
    named.name = format!("build\u{1b}]52;c;aGk=\u{7}\r{}", "x".repeat(400));
    named.workflow = Some("\u{1b}[31mCI\u{1b}[0m".to_owned());
    let timeline = SlotTimeline::of(&[named], WINDOW, vec![], 0);
    let bar = &timeline.unassigned[0];
    assert!(
        !bar.name.chars().any(char::is_control) && bar.name.len() <= 200,
        "{:?}",
        bar.name
    );
    assert_eq!(bar.workflow.as_deref(), Some("CI"));
}

#[test]
fn a_window_is_refused_backwards_or_past_its_longest() {
    let now = 10 * TIMELINE_WINDOW_MAX;
    assert_eq!(
        TimelineQuery::default().window(now),
        Ok(Span {
            from: now - TIMELINE_DEFAULT,
            to: now
        })
    );
    let until_later = TimelineQuery {
        since: Some(now - 10),
        until: Some(now + 99),
    };
    assert_eq!(
        until_later.window(now).unwrap().to,
        now,
        "a future end is now"
    );
    for refused in [
        TimelineQuery {
            since: Some(now),
            until: Some(now),
        },
        TimelineQuery {
            since: Some(now - TIMELINE_WINDOW_MAX - 1),
            until: None,
        },
    ] {
        assert!(refused.window(now).is_err(), "{refused:?}");
    }
}

#[test]
fn a_newer_daemons_end_reads_as_other() {
    let end: BarEnd = serde_json::from_str(r#"{"kind": "estimated", "at": 5}"#).unwrap();
    assert_eq!(end, BarEnd::Other);
    assert_eq!(end.at(), None);
}
