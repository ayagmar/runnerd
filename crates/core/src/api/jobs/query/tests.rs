use super::*;
use crate::api::{JobMetrics, JobState, LogSummary, Timeline};

fn job(id: u64, queued_at: u64) -> JobSummary {
    JobSummary {
        id,
        repository: "halyard/ledger".to_owned(),
        run_id: 1,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: "build".to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        labels: vec![],
        state: JobState::Completed,
        conclusion: Some("success".to_owned()),
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

fn ran(id: u64, queued_at: u64, started: Option<u64>, finished: Option<u64>) -> JobSummary {
    let mut job = job(id, queued_at);
    job.timeline.started_at = started;
    job.timeline.finished_at = finished;
    job
}

fn ids(list: &JobList) -> Vec<u64> {
    list.page.jobs.iter().map(|job| job.id).collect()
}

/// Every page of `query` over `jobs`, following each page's cursor or `next`.
fn pages(query: &JobQuery, jobs: &[JobSummary]) -> Vec<Vec<u64>> {
    let mut query = query.clone();
    let mut pages = Vec::new();
    loop {
        let list = query.page(jobs).unwrap();
        pages.push(ids(&list));
        match (list.cursor, list.page.next) {
            (Some(cursor), _) => query.cursor = Some(cursor),
            (None, Some(next)) => query.before = Some(next),
            (None, None) => return pages,
        }
    }
}

#[test]
fn a_sort_puts_unknown_keys_last_either_way_and_breaks_ties_by_id_across_pages() {
    // Waits: 1 and 2 waited 10s, 3 waited 5s, 4 and 5 never started.
    let jobs = vec![
        ran(1, 100, Some(110), Some(200)),
        ran(2, 100, Some(110), Some(150)),
        ran(3, 100, Some(105), Some(300)),
        ran(4, 100, None, None),
        ran(5, 100, None, None),
    ];
    let wait = |order| JobQuery {
        sort: Some(JobSort::Wait),
        order,
        limit: Some(1),
        ..JobQuery::default()
    };
    assert_eq!(
        pages(&wait(None), &jobs).concat(),
        [2, 1, 3, 5, 4],
        "desc: largest first, ties by the larger ID, unknown last"
    );
    assert_eq!(
        pages(&wait(Some(SortOrder::Asc)), &jobs).concat(),
        [3, 1, 2, 4, 5],
        "asc: smallest first, ties by the smaller ID, unknown still last"
    );
    for limit in 1..=6 {
        let query = JobQuery {
            limit: Some(limit),
            ..wait(None)
        };
        assert_eq!(
            pages(&query, &jobs).concat(),
            [2, 1, 3, 5, 4],
            "pages of {limit}"
        );
    }
}

#[test]
fn run_time_and_memory_are_unknown_until_a_jobs_record_is_final() {
    let mut running = ran(1, 100, Some(110), None);
    running.state = JobState::Running;
    running.metrics.memory_peak = Some(9 << 30);
    let mut done = ran(2, 100, Some(110), Some(130));
    done.metrics.memory_peak = Some(1 << 30);
    assert_eq!(
        (JobSort::Run.key(&running), JobSort::Memory.key(&running)),
        (None, None),
        "a running job's run time and peak still grow"
    );
    assert_eq!(
        (JobSort::Run.key(&done), JobSort::Memory.key(&done)),
        (Some(20), Some(1 << 30))
    );
    assert_eq!(JobSort::Wait.key(&running), Some(10), "its wait is over");
}

#[test]
fn a_key_that_becomes_known_never_shows_a_job_twice_in_one_pass() {
    let mut jobs: Vec<JobSummary> = (1..=6)
        .map(|id| ran(id, 100, Some(100 + id), None))
        .collect();
    for job in &mut jobs {
        job.state = JobState::Running;
    }
    jobs[0].state = JobState::Completed;
    jobs[0].timeline.finished_at = Some(500);
    let query = JobQuery {
        sort: Some(JobSort::Run),
        limit: Some(2),
        ..JobQuery::default()
    };
    let first = query.page(&jobs).unwrap();
    // Between pages, every running job ends: their keys go from unknown to known.
    for job in &mut jobs {
        job.state = JobState::Completed;
        job.timeline.finished_at.get_or_insert(1000);
    }
    let mut seen = ids(&first);
    let mut query = query.clone();
    query.cursor = first.cursor;
    while query.cursor.is_some() {
        let list = query.page(&jobs).unwrap();
        seen.extend(ids(&list));
        query.cursor = list.cursor;
    }
    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "no job twice: {seen:?}");
}

#[test]
fn a_cursor_pages_only_the_sort_order_and_filters_it_was_made_for() {
    let jobs: Vec<JobSummary> = (1..=4).map(|id| job(id, 100 + id)).collect();
    let query = JobQuery {
        sort: Some(JobSort::Queued),
        limit: Some(1),
        ..JobQuery::default()
    };
    let cursor = query.page(&jobs).unwrap().cursor;
    assert!(cursor.is_some(), "a first page of one has a next");
    let changed = [
        JobQuery {
            q: Some("build".to_owned()),
            ..query.clone()
        },
        JobQuery {
            branch: Some("main".to_owned()),
            ..query.clone()
        },
        JobQuery {
            order: Some(SortOrder::Asc),
            ..query.clone()
        },
        JobQuery {
            sort: Some(JobSort::Wait),
            ..query.clone()
        },
    ];
    for changed in changed {
        let changed = JobQuery {
            cursor: cursor.clone(),
            ..changed
        };
        assert_eq!(
            changed.check().unwrap_err(),
            "the cursor is of another sort or search; start from the first page"
        );
    }
    let same = JobQuery {
        cursor: cursor.clone(),
        limit: Some(3),
        ..query.clone()
    };
    assert!(same.check().is_ok(), "a page size is not a filter");
    for refused in [
        JobQuery {
            cursor: Some("j1.wait.desc.0.-.4".to_owned()),
            ..JobQuery::default()
        },
        JobQuery {
            before: Some(3),
            ..query.clone()
        },
        JobQuery {
            cursor: Some("j2.queued.desc.0.1.4".to_owned()),
            ..query.clone()
        },
        JobQuery {
            cursor: Some("j1.queued.desc.zz.1.4".to_owned()),
            ..query.clone()
        },
    ] {
        assert!(refused.check().is_err(), "{refused:?}");
    }
}

#[test]
fn a_cursor_reads_back_what_it_wrote() {
    for key in [None, Some(0), Some(u64::MAX)] {
        let cursor = JobCursor {
            sort: JobSort::Memory,
            order: SortOrder::Asc,
            filter: 0xdead_beef,
            key,
            id: 42,
        };
        assert_eq!(JobCursor::parse(&cursor.encode()), Some(cursor));
    }
    for text in [
        "",
        "j1",
        "j1.memory.asc.1.2.3.4",
        "j1.disk.asc.1.2.3",
        "j1.memory.up.1.2.3",
    ] {
        assert_eq!(JobCursor::parse(text), None, "{text}");
    }
}

#[test]
fn a_search_is_literal_over_name_workflow_and_branch_in_either_ascii_case() {
    let mut jobs = vec![job(1, 1), job(2, 2), job(3, 3), job(4, 4)];
    jobs[0].name = "test (100%_done)".to_owned();
    jobs[1].workflow = Some("Release".to_owned());
    jobs[2].branch = Some("feature/Login".to_owned());
    jobs[3].workflow = None;
    jobs[3].branch = None;
    let found = |q: &str| {
        let query = JobQuery {
            q: Some(q.to_owned()),
            ..JobQuery::default()
        };
        ids(&query.page(&jobs).unwrap())
    };
    assert_eq!(found("%_d"), [1], "% and _ are text");
    assert_eq!(found("100%"), [1]);
    assert_eq!(found("RELEASE"), [2]);
    assert_eq!(found("login"), [3]);
    assert_eq!(found("x%"), Vec::<u64>::new());
    for q in ["", &"x".repeat(JOB_SEARCH_MAX + 1), "a\u{7}b"] {
        let query = JobQuery {
            q: Some(q.to_owned()),
            ..JobQuery::default()
        };
        assert!(query.check().is_err(), "{q:?} is refused");
    }
}

#[test]
fn a_record_filter_and_mark_follow_the_records_note() {
    let note = |skipped: bool, removed: bool| RecordNote {
        created_at: (!skipped).then_some(1),
        bytes: None,
        skipped: skipped.then(|| "too little free disk".to_owned()),
        removed_at: removed.then_some(2),
        removed_by: None,
    };
    assert_eq!(RecordMark::of(&note(false, false)), RecordMark::Kept);
    assert_eq!(RecordMark::of(&note(false, true)), RecordMark::Removed);
    assert_eq!(RecordMark::of(&note(true, true)), RecordMark::Skipped);
    let marks = [
        None,
        Some(RecordMark::Kept),
        Some(RecordMark::Removed),
        Some(RecordMark::Skipped),
        Some(RecordMark::Unknown),
    ];
    let admitted = |filter: RecordFilter| -> Vec<Option<RecordMark>> {
        marks
            .into_iter()
            .filter(|mark| filter.admits(*mark))
            .collect()
    };
    assert_eq!(admitted(RecordFilter::Kept), [Some(RecordMark::Kept)]);
    assert_eq!(
        admitted(RecordFilter::Any).len(),
        4,
        "any note, a newer one's too"
    );
    assert_eq!(RecordMark::Kept.words(), Some("record kept"));
    assert_eq!(RecordMark::Unknown.words(), None);
}

/// `Job` flattens its summary: the mark and the job's own record note must not share a key.
#[test]
fn a_jobs_record_note_and_its_summarys_mark_both_survive_its_json() {
    let note = RecordNote {
        created_at: Some(1),
        bytes: Some(2),
        skipped: None,
        removed_at: None,
        removed_by: None,
    };
    let mut summary = job(1, 10);
    summary.record_mark = Some(RecordMark::of(&note));
    let job = crate::api::Job {
        summary,
        steps: Vec::new(),
        record: Some(note),
        admission: None,
    };
    let text = serde_json::to_string(&job).unwrap();
    assert_eq!(serde_json::from_str::<crate::api::Job>(&text).unwrap(), job);
}

#[test]
fn a_newer_daemons_record_mark_reads_as_unknown_and_an_older_daemons_list_as_not_understood() {
    let mark: RecordMark = serde_json::from_str("\"archived\"").unwrap();
    assert_eq!(mark, RecordMark::Unknown);
    // `GET /v1/jobs` as runnerd 0.1.0 answered it, which ignored `q` and `sort`.
    let old: JobList = serde_json::from_str(r#"{"jobs": [], "next": 7}"#).unwrap();
    assert!(
        !old.understood(),
        "an old daemon's list is not a search result"
    );
    assert_eq!(old.page.next, Some(7));
    let new = JobQuery::default().page(&[]).unwrap();
    assert!(new.understood(), "this daemon says what it read");
}

#[test]
fn a_page_ends_at_its_byte_budget_and_says_there_is_more() {
    let mut big = job(1, 1);
    big.name = "x".repeat(usize::try_from(JOBS_PAGE_BYTES).unwrap());
    let jobs = vec![big, job(2, 2), job(3, 3)];
    let list = JobQuery::default().page(&jobs).unwrap();
    assert_eq!(
        ids(&list),
        [3, 2, 1],
        "the job that reaches the budget is the last"
    );
    let mut first_big = jobs.clone();
    first_big[2].name = "y".repeat(usize::try_from(JOBS_PAGE_BYTES).unwrap());
    let list = JobQuery::default().page(&first_big).unwrap();
    assert_eq!((ids(&list), list.page.next), (vec![3], Some(3)));
}
