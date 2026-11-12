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

/// Every page of `query` over `jobs` in the default order, following each page's `next`.
fn pages(query: &JobQuery, jobs: &[JobSummary]) -> Vec<Vec<u64>> {
    let mut query = query.clone();
    let mut pages = Vec::new();
    loop {
        let list = query.page(jobs).unwrap();
        pages.push(ids(&list));
        match list.page.next {
            Some(next) => query.before = Some(next),
            None => return pages,
        }
    }
}

#[test]
fn a_sort_puts_unknown_keys_last_either_way_and_breaks_ties_by_id() {
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
        ..JobQuery::default()
    };
    assert_eq!(
        ids(&wait(None).page(&jobs).unwrap()),
        [2, 1, 3, 5, 4],
        "desc: largest first, ties by the larger ID, unknown last"
    );
    assert_eq!(
        ids(&wait(Some(SortOrder::Asc)).page(&jobs).unwrap()),
        [3, 1, 2, 4, 5],
        "asc: smallest first, ties by the smaller ID, unknown still last"
    );
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
fn a_sorted_list_is_one_page_that_says_how_many_the_filters_admit() {
    // Job N queued at 100 + N and waited N seconds.
    let jobs: Vec<JobSummary> = (1..=6)
        .map(|id| ran(id, 100 + id, Some(100 + 2 * id), Some(300)))
        .collect();
    let longest = JobQuery {
        sort: Some(JobSort::Wait),
        limit: Some(2),
        ..JobQuery::default()
    };
    let list = longest.page(&jobs).unwrap();
    assert_eq!(ids(&list), [6, 5]);
    assert_eq!(
        (list.page.next, list.total),
        (None, Some(6)),
        "no continuation"
    );
    assert_eq!(
        list.shown_words().as_deref(),
        Some("2 of 6 jobs, longest wait first")
    );
    let narrowed = JobQuery {
        until: Some(103),
        ..longest.clone()
    };
    assert_eq!(
        narrowed.page(&jobs).unwrap().shown_words(),
        None,
        "the filters narrow it to what one page holds"
    );
    let oldest = JobQuery {
        sort: Some(JobSort::Queued),
        order: Some(SortOrder::Asc),
        limit: Some(4),
        ..JobQuery::default()
    };
    let list = oldest.page(&jobs).unwrap();
    assert_eq!((ids(&list), list.page.next), (vec![1, 2, 3, 4], None));
    assert_eq!(
        list.shown_words().as_deref(),
        Some("4 of 6 jobs, oldest first")
    );

    // Newest first is the default order, sorted or not, and pages by ID.
    for sort in [None, Some(JobSort::Queued)] {
        let query = JobQuery {
            sort,
            limit: Some(4),
            ..JobQuery::default()
        };
        assert_eq!(pages(&query, &jobs), [vec![6, 5, 4, 3], vec![2, 1]]);
        let list = query.page(&jobs).unwrap();
        assert_eq!((list.total, list.shown_words()), (None, None));
    }
}

#[test]
fn a_cursor_and_before_outside_the_default_order_are_refused_in_cores_words() {
    let wait = JobQuery {
        sort: Some(JobSort::Wait),
        ..JobQuery::default()
    };
    assert_eq!(
        JobQuery {
            cursor: Some("j1.wait.desc.6ac9806bd2bf4ce5.0.0".to_owned()),
            ..wait.clone()
        }
        .check()
        .unwrap_err(),
        "a sorted list is one page, its first jobs; narrow it with a search or the filters"
    );
    assert!(
        JobQuery {
            before: Some(3),
            ..wait.clone()
        }
        .check()
        .unwrap_err()
        .starts_with("before pages the list newest first"),
        "before with a sort"
    );
    assert!(
        JobQuery {
            before: Some(3),
            order: Some(SortOrder::Asc),
            ..JobQuery::default()
        }
        .check()
        .is_err(),
        "before oldest first"
    );
    assert_eq!(
        JobQuery {
            before: Some(3),
            sort: Some(JobSort::Queued),
            ..JobQuery::default()
        }
        .check(),
        Ok(()),
        "before newest first"
    );
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
        gone_at: None,
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
        gone_at: None,
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
fn a_jobs_long_text_is_cut_as_a_list_shows_it_and_a_page_ends_before_its_byte_budget() {
    let mut big = job(1, 1);
    big.name = "x".repeat(usize::try_from(JOBS_PAGE_BYTES).unwrap() * 8);
    big.labels = vec!["l".repeat(JOB_LABELS_MAX)];
    let jobs = vec![big, job(2, 2)];
    let list = JobQuery::default().page(&jobs).unwrap();
    assert_eq!(
        ids(&list),
        [2, 1],
        "a job past the budget alone is shown, cut"
    );
    let shown = &list.page.jobs[1];
    assert_eq!(shown.name.len(), JOB_TEXT_MAX);
    assert!(
        shown.labels.is_empty(),
        "labels past their bound are left out"
    );
    assert_eq!(list.shortened, [1]);

    // Each job's text cut to a quarter of a megabyte's worth across 4-byte characters: a page ends before the job
    // that would pass the budget, and the next starts at it.
    let wide: Vec<JobSummary> = (1..=500)
        .map(|id| {
            let mut wide = job(id, id);
            wide.name = "\u{1d11e}".repeat(JOB_TEXT_MAX * 2);
            wide.workflow = Some(wide.name.clone());
            wide.branch = Some(wide.name.clone());
            wide
        })
        .collect();
    let query = JobQuery {
        limit: Some(JOBS_PAGE_MAX),
        ..JobQuery::default()
    };
    let first = query.page(&wide).unwrap();
    let bytes: u64 = first.page.jobs.iter().map(row_bytes).sum();
    assert!(bytes <= JOBS_PAGE_BYTES, "{bytes} bytes");
    let row = row_bytes(&first.page.jobs[0]);
    assert!(
        bytes + row > JOBS_PAGE_BYTES,
        "one more would have passed it"
    );
    let count = u64::try_from(first.page.jobs.len()).unwrap();
    assert_eq!(first.page.next, Some(501 - count));
    let all = pages(&query, &wide).concat();
    assert_eq!(all, (1..=500).rev().collect::<Vec<_>>(), "every job once");
}
