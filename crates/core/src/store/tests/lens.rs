//! The history's search, sorts, timeline and compare (ADR 0135): the store's SQL gives the pages, lanes and pairs
//! `api` defines, over generated histories with ties, unknown keys, record notes and restarts; each response is of
//! one moment; and a history an older runnerd wrote reads the same.

use std::path::Path;

use rusqlite::{Connection, params};

use super::{keep_since, user_version};
use crate::api::{
    self, BarEnd, Baseline, FigureKind, GapKind, JobComparison, JobList, JobQuery, JobSort,
    JobSummary, RecordFilter, RecordMark, SlotTimeline, SortOrder, Span, StepMatch,
};
use crate::store::{Store, VERSION};
use crate::testdir::TestDir;

/// The schema at version 21, as runnerd 0.0.23 to 0.1.0 created it.
const SCHEMA_21: &str = include_str!("schema-21.sql");

/// 2026-10-23T01:00:00Z, in seconds.
const T0: u64 = 1_792_717_200;

/// A small random number generator with a fixed seed, so a failing history is the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below
    }

    fn pick<'item, T>(&mut self, items: &'item [T]) -> &'item T {
        &items[usize::try_from(self.next(items.len() as u64)).unwrap()]
    }
}

/// A job row as the history holds it, times in ms.
#[derive(Clone)]
struct Row {
    id: u64,
    repository: &'static str,
    workflow: Option<&'static str>,
    name: &'static str,
    branch: Option<&'static str>,
    state: &'static str,
    conclusion: Option<&'static str>,
    queued_at: u64,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    memory_peak: Option<u64>,
    /// `profile`, `slot`, container start and removal (ms).
    runner: Option<(&'static str, usize, u64, Option<u64>)>,
}

fn insert(db: &Connection, row: &Row) {
    let runner = row.runner.map(|_| format!("r{}", row.id));
    db.execute(
        "INSERT INTO jobs (id, repository, run_id, run_attempt, workflow, name, branch, labels, state, conclusion, \
         runner, queued_at, started_at, finished_at, memory_peak) \
         VALUES (?1, ?2, ?1, 1, ?3, ?4, ?5, '[\"self-hosted\"]', ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            row.id,
            row.repository,
            row.workflow,
            row.name,
            row.branch,
            row.state,
            row.conclusion,
            runner,
            row.queued_at,
            row.started_at,
            row.finished_at,
            row.memory_peak
        ],
    )
    .unwrap();
    if let (Some(name), Some((profile, slot, started, removed))) = (runner, row.runner) {
        db.execute(
            "INSERT INTO runners (name, repository, profile, slot, container, started_at, removed_at) \
             VALUES (?1, ?2, ?3, ?4, ?1, ?5, ?6)",
            params![name, row.repository, profile, slot, started, removed],
        )
        .unwrap();
    }
}

/// `count` jobs with many equal and unknown keys, odd names and every record note, times with milliseconds.
fn generated(seed: u64, count: u64) -> Vec<Row> {
    let mut rng = Rng(seed);
    (1..=count)
        .map(|id| {
            let state = *rng.pick(&[
                "completed",
                "completed",
                "running",
                "queued",
                "lost",
                "elsewhere",
            ]);
            let queued_at = (T0 + rng.next(4) * 10) * 1000 + rng.next(1000);
            let started_at = match state {
                "queued" => None,
                "elsewhere" if rng.next(2) == 0 => None,
                _ => Some(queued_at + rng.next(3) * 5000 + rng.next(1000)),
            };
            let finished_at = match (state, started_at) {
                ("running" | "queued", _) => None,
                ("lost", _) if rng.next(2) == 0 => None,
                (_, Some(started)) => Some(started + rng.next(3) * 7000 + rng.next(1000)),
                (_, None) => Some(queued_at + 1000),
            };
            Row {
                id,
                repository: rng.pick(&["o/a", "o/b"]),
                workflow: *rng.pick(&[Some("CI"), None, Some("Re%lease")]),
                name: rng.pick(&["build", "b_uild", "baulk", "engel"]),
                branch: *rng.pick(&[Some("main"), None, Some("feat/A")]),
                state,
                conclusion: (state == "completed").then(|| *rng.pick(&["success", "failure"])),
                queued_at,
                started_at,
                finished_at,
                memory_peak: (rng.next(3) > 0).then(|| rng.next(3) << 30),
                runner: started_at.filter(|_| state != "elsewhere").map(|started| {
                    (
                        *rng.pick(&["jvm", "node"]),
                        1 + usize::try_from(rng.next(2)).unwrap(),
                        started,
                        finished_at.map(|at| at + 500),
                    )
                }),
            }
        })
        .collect()
}

/// A note for each kind of record, on some of the jobs.
fn notes(db: &Connection, count: u64) {
    for id in (1..=count).step_by(3) {
        let sql = match id % 4 {
            0 => "INSERT INTO records (job, created_at, bytes) VALUES (?1, 1000, 10)",
            1 => {
                "INSERT INTO records (job, created_at, bytes, removed_at, removed_by) VALUES (?1, 1, 1, 2, 'age')"
            }
            2 => "INSERT INTO records (job, skipped) VALUES (?1, 'too little free disk')",
            _ => "INSERT INTO records (job, removed_at, removed_by) VALUES (?1, 2, 'user')",
        };
        db.execute(sql, [id]).unwrap();
    }
}

/// Every page of `query`, following each cursor or `next`, reopening the store after every other page.
async fn store_pages(path: &Path, query: &JobQuery) -> Vec<Vec<u64>> {
    let mut store = Store::open(path).unwrap();
    let mut query = query.clone();
    let mut pages = Vec::new();
    loop {
        let list = store.jobs(query.clone()).await.unwrap();
        pages.push(list.page.jobs.iter().map(|job| job.id).collect());
        if pages.len() % 2 == 0 {
            drop(store);
            store = Store::open(path).unwrap();
        }
        match (list.cursor, list.page.next) {
            (Some(cursor), _) => query.cursor = Some(cursor),
            (None, Some(next)) => query.before = Some(next),
            (None, None) => return pages,
        }
    }
}

/// The same pages as `api` defines them over the jobs the store reads.
fn core_pages(query: &JobQuery, jobs: &[JobSummary]) -> Vec<Vec<u64>> {
    let mut query = query.clone();
    let mut pages = Vec::new();
    loop {
        let list: JobList = query.page(jobs).unwrap();
        pages.push(list.page.jobs.iter().map(|job| job.id).collect());
        match (list.cursor, list.page.next) {
            (Some(cursor), _) => query.cursor = Some(cursor),
            (None, Some(next)) => query.before = Some(next),
            (None, None) => return pages,
        }
    }
}

async fn every_job(store: &Store) -> Vec<JobSummary> {
    let query = JobQuery {
        limit: Some(api::JOBS_PAGE_MAX),
        ..JobQuery::default()
    };
    store.jobs(query).await.unwrap().page.jobs
}

#[tokio::test]
async fn the_historys_pages_are_the_ones_core_defines_for_every_search_sort_and_filter_across_restarts()
 {
    const COUNT: u64 = 90;
    let dir = TestDir::new("lens-pages");
    let path = dir.0.join("runnerd.db");
    drop(Store::open(&path).unwrap());
    for seed in [7, 1_000_003] {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("DELETE FROM jobs; DELETE FROM runners; DELETE FROM records;")
            .unwrap();
        for row in generated(seed, COUNT) {
            insert(&db, &row);
        }
        notes(&db, COUNT);
        drop(db);
        let store = Store::open(&path).unwrap();
        let jobs = every_job(&store).await;
        assert_eq!(jobs.len() as u64, COUNT);
        for job in &jobs {
            let note = store.job(job.id).await.unwrap().unwrap().record;
            assert_eq!(
                job.record_mark,
                note.as_ref().map(RecordMark::of),
                "job {}'s mark is its note's state",
                job.id
            );
        }
        drop(store);
        let filters = [
            JobQuery::default(),
            JobQuery {
                q: Some("b_u".to_owned()),
                ..JobQuery::default()
            },
            JobQuery {
                q: Some("E%L".to_owned()),
                ..JobQuery::default()
            },
            JobQuery {
                q: Some("feat/a".to_owned()),
                repository: Some("o/a".to_owned()),
                ..JobQuery::default()
            },
            JobQuery {
                branch: Some("main".to_owned()),
                record: Some(RecordFilter::Any),
                ..JobQuery::default()
            },
            JobQuery {
                record: Some(RecordFilter::Skipped),
                ..JobQuery::default()
            },
            JobQuery {
                state: Some(api::JobState::Completed),
                since: Some(T0 + 10),
                until: Some(T0 + 30),
                ..JobQuery::default()
            },
        ];
        let sorts = [
            None,
            Some(JobSort::Queued),
            Some(JobSort::Wait),
            Some(JobSort::Run),
            Some(JobSort::Memory),
        ];
        for filter in &filters {
            for sort in sorts {
                for order in [None, Some(SortOrder::Asc)] {
                    for limit in [1, 4, 500] {
                        let query = JobQuery {
                            sort,
                            order: sort.and(order),
                            limit: Some(limit),
                            ..filter.clone()
                        };
                        let expected = core_pages(&query, &jobs);
                        assert_eq!(store_pages(&path, &query).await, expected, "{query:?}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn a_page_is_read_in_one_snapshot_while_jobs_are_pruned() {
    let dir = TestDir::new("lens-snapshot");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    let db = Connection::open(&path).unwrap();
    for row in generated(3, 50) {
        insert(&db, &row);
    }
    drop(db);
    // Pruning deletes old jobs, a batch at a time. Here the first half goes and comes back, again and again.
    let writer = path.clone();
    let pruner = tokio::task::spawn_blocking(move || {
        let db = Connection::open(&writer).unwrap();
        db.execute_batch("CREATE TEMP TABLE first AS SELECT * FROM jobs WHERE id <= 25")
            .unwrap();
        for _ in 0..300 {
            db.execute_batch(
                "DELETE FROM jobs WHERE id <= 25; INSERT INTO jobs SELECT * FROM first;",
            )
            .unwrap();
        }
    });
    let mut reads = 0;
    while !pruner.is_finished() {
        let list = store
            .jobs(JobQuery {
                limit: Some(50),
                sort: Some(JobSort::Queued),
                ..JobQuery::default()
            })
            .await
            .unwrap();
        let count = list.page.jobs.len();
        assert!(
            count == 25 || count == 50,
            "a page holds the jobs of one moment, not {count}"
        );
        reads += 1;
    }
    pruner.await.unwrap();
    assert!(reads > 0, "the jobs were read while they were pruned");
}

/// A host sample every 2 s from `from` to `to`, in seconds.
fn watched(db: &Connection, from: u64, to: u64) {
    for at in (from..=to).step_by(2) {
        db.execute(
            "INSERT INTO host_samples (at, cpu, memory) VALUES (?1, 0, 0)",
            [at * 1000],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn a_timeline_is_the_one_core_lays_out_from_the_jobs_overlapping_its_window() {
    let dir = TestDir::new("lens-timeline");
    let path = dir.0.join("runnerd.db");
    drop(Store::open(&path).unwrap());
    let db = Connection::open(&path).unwrap();
    let rows = generated(11, 80);
    for row in &rows {
        insert(&db, row);
    }
    keep_since(&path, 0);
    watched(&db, T0, T0 + 12);
    // A restart: no sample from T0 + 12 to T0 + 20.
    watched(&db, T0 + 20, T0 + 40);
    drop(db);
    let store = Store::open(&path).unwrap();
    let jobs = every_job(&store).await;
    for window in [
        Span {
            from: T0,
            to: T0 + 25,
        },
        Span {
            from: T0 + 12,
            to: T0 + 15,
        },
        Span {
            from: T0 + 30,
            to: T0 + 3600,
        },
    ] {
        let read = store.timeline(window).await.unwrap();
        let laid = SlotTimeline::of(&jobs, window, read.observed.clone(), read.history_from);
        let bars = |timeline: &SlotTimeline| {
            let lanes: Vec<(String, usize, Vec<u64>)> = timeline
                .lanes
                .iter()
                .map(|lane| {
                    (
                        lane.profile.clone(),
                        lane.slot,
                        lane.bars.iter().map(|bar| bar.job).collect::<Vec<u64>>(),
                    )
                })
                .filter(|lane| !lane.2.is_empty())
                .collect();
            (lanes, timeline.unassigned.clone())
        };
        assert_eq!(bars(&read), bars(&laid), "{window:?}");
    }
    let read = store
        .timeline(Span {
            from: T0,
            to: T0 + 25,
        })
        .await
        .unwrap();
    assert_eq!(
        read.observed,
        [
            Span {
                from: T0,
                to: T0 + 12
            },
            Span {
                from: T0 + 20,
                to: T0 + 24
            }
        ],
        "the restart is a gap"
    );
}

/// A finished job of `o/r` in `profile`'s `slot`, which ran from `from` to `to` (seconds); its runner was up a
/// second around it.
fn slot_job(id: u64, profile: &'static str, slot: usize, from: u64, to: Option<u64>) -> Row {
    Row {
        id,
        repository: "o/r",
        workflow: Some("CI"),
        name: "build",
        branch: Some("main"),
        state: if to.is_some() { "completed" } else { "running" },
        conclusion: to.map(|_| "success"),
        queued_at: (from - 1) * 1000,
        started_at: Some(from * 1000),
        finished_at: to.map(|to| to * 1000),
        memory_peak: None,
        runner: Some((
            profile,
            slot,
            (from - 1) * 1000,
            to.map(|to| (to + 1) * 1000),
        )),
    }
}

#[tokio::test]
async fn an_empty_historys_timeline_has_nothing_and_watched_nothing() {
    let dir = TestDir::new("lens-empty");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    let window = Span {
        from: T0,
        to: T0 + 100,
    };
    let empty = store.timeline(window).await.unwrap();
    assert_eq!(
        empty,
        SlotTimeline {
            since: T0,
            until: T0 + 100,
            history_from: empty.history_from,
            ..SlotTimeline::default()
        }
    );
}

/// Jobs of slots a reload took away, a lost and a waiting job, a warm runner, and samples around a restart, some
/// held.
fn slots_history(store: &Store, path: &Path) {
    let db = Connection::open(path).unwrap();
    let mut lost = slot_job(4, "jvm", 2, T0 + 150, None);
    lost.state = "lost";
    let mut waiting = slot_job(5, "jvm", 1, T0 + 170, None);
    (waiting.state, waiting.started_at, waiting.runner) = ("queued", None, None);
    for row in [
        slot_job(1, "jvm", 1, T0 + 90, Some(T0 + 120)),
        // A third slot a later reload took away, and a profile it removed.
        slot_job(2, "jvm", 3, T0 + 190, Some(T0 + 400)),
        slot_job(3, "retired", 1, T0 + 130, Some(T0 + 140)),
        lost,
        waiting,
    ] {
        insert(&db, &row);
    }
    // A warm runner up with no job.
    db.execute(
        "INSERT INTO runners (name, repository, profile, slot, container, started_at, purpose) \
         VALUES ('warm', 'o/r', 'jvm', 1, 'warm', ?1, 'warm')",
        [(T0 + 125) * 1000],
    )
    .unwrap();
    watched(&db, T0 + 100, T0 + 140);
    watched(&db, T0 + 160, T0 + 180);
    drop(db);
    keep_since(path, (T0 + 110) * 1000);
    // Samples not stored yet are watched time too.
    for at in (T0 + 182..T0 + 190).step_by(2) {
        store.host_sample(api::HostSample {
            at,
            cpu: 0,
            memory: 0,
        });
    }
}

#[tokio::test]
async fn a_timeline_keeps_old_slots_real_ends_restarts_and_unknown_time_apart() {
    let dir = TestDir::new("lens-slots");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    let window = Span {
        from: T0 + 100,
        to: T0 + 200,
    };
    slots_history(&store, &path);
    let timeline = store.timeline(window).await.unwrap();
    let lane = |profile: &str, slot: usize| {
        timeline
            .lanes
            .iter()
            .find(|lane| (lane.profile.as_str(), lane.slot) == (profile, slot))
            .unwrap()
    };
    assert_eq!(timeline.history_from, T0 + 110);
    assert_eq!(
        timeline.observed,
        [
            Span {
                from: T0 + 110,
                to: T0 + 140
            },
            Span {
                from: T0 + 160,
                to: T0 + 188
            },
        ],
        "watched from the history's start, around the restart, the held samples too"
    );
    let first = lane("jvm", 1);
    assert_eq!(
        (first.bars[0].started_at, first.bars[0].end),
        (Some(T0 + 90), BarEnd::At { at: T0 + 120 }),
        "a bar keeps its real ends"
    );
    assert_eq!(
        first
            .runners
            .iter()
            .map(|runner| (runner.name.as_str(), runner.end))
            .collect::<Vec<_>>(),
        [("r1", BarEnd::At { at: T0 + 121 }), ("warm", BarEnd::Open)]
    );
    assert_eq!(lane("jvm", 3).bars[0].end, BarEnd::At { at: T0 + 400 });
    assert_eq!(lane("retired", 1).bars.len(), 1);
    assert_eq!(lane("jvm", 2).bars[0].end_words(), Some("end unknown"));
    assert_eq!(
        timeline
            .unassigned
            .iter()
            .map(|bar| bar.job)
            .collect::<Vec<_>>(),
        [5]
    );
    let gaps: Vec<(u64, u64, GapKind)> = first
        .gaps(&timeline)
        .into_iter()
        .map(|gap| (gap.span.from - T0, gap.span.to - T0, gap.kind))
        .collect();
    assert_eq!(
        gaps,
        [
            (120, 140, GapKind::NoJob),
            (140, 160, GapKind::Unknown),
            (160, 188, GapKind::NoJob),
            (188, 200, GapKind::Unknown),
        ],
        "a warm runner is no job; unwatched time is unknown, never idle"
    );
}

#[tokio::test]
async fn a_timeline_past_its_cap_keeps_the_latest_and_says_from_when() {
    let dir = TestDir::new("lens-cap");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    let db = Connection::open(&path).unwrap();
    let count = api::TIMELINE_BARS_MAX as u64 + 5;
    for id in 1..=count {
        insert(&db, &slot_job(id, "jvm", 1, T0 + id, Some(T0 + id + 1)));
    }
    drop(db);
    let timeline = store
        .timeline(Span {
            from: T0,
            to: T0 + 10_000,
        })
        .await
        .unwrap();
    assert_eq!(timeline.lanes[0].bars.len(), api::TIMELINE_BARS_MAX);
    assert_eq!(timeline.lanes[0].runners.len(), api::TIMELINE_BARS_MAX);
    assert_eq!(
        timeline.lanes[0].bars[0].job, 6,
        "the oldest five are left out"
    );
    assert_eq!(timeline.cut_before, Some(T0 + 5), "job 6 was queued then");
}

/// Job `id`'s steps, in seconds: the set-up, a tests step of `tests` seconds, and two named `Run lint`.
fn steps(db: &Connection, id: u64, from: u64, tests: u64, renamed: bool) {
    let name = if renamed {
        "Run unit tests"
    } else {
        "Run tests"
    };
    for (number, name, start, end) in [
        (1, "Set up job", from, from + 2),
        (2, name, from + 2, from + 2 + tests),
        (3, "Run lint", from + 2 + tests, from + 4 + tests),
        (4, "Run lint", from + 4 + tests, from + 6 + tests),
    ] {
        db.execute(
            "INSERT INTO steps (job, number, name, status, conclusion, started_at, completed_at) \
             VALUES (?1, ?2, ?3, 'completed', 'success', ?4, ?5)",
            params![id, number, name, start * 1000, end * 1000],
        )
        .unwrap();
    }
}

/// Runs of job `build` on `main` (1, 2 and 5), a failed one, one on `dev` and a job of another name, each with a
/// tests step and two steps named `Run lint`; job 5's and job 2's tests steps sampled, job 1's samples pruned.
fn compare_history(path: &Path) {
    let db = Connection::open(path).unwrap();
    let mut failed = slot_job(3, "jvm", 1, T0 + 300, Some(T0 + 400));
    failed.conclusion = Some("failure");
    let mut other_branch = slot_job(4, "jvm", 1, T0 + 500, Some(T0 + 600));
    other_branch.branch = Some("dev");
    let mut other_name = slot_job(6, "jvm", 1, T0 + 700, Some(T0 + 800));
    other_name.name = "lint";
    for (row, tests) in [
        (slot_job(1, "jvm", 1, T0, Some(T0 + 100)), 60),
        (slot_job(2, "jvm", 1, T0 + 100, Some(T0 + 200)), 60),
        (failed, 60),
        (other_branch, 60),
        (slot_job(5, "jvm", 2, T0 + 1000, Some(T0 + 1100)), 80),
        (other_name, 60),
    ] {
        let from = row.started_at.unwrap() / 1000;
        insert(&db, &row);
        steps(&db, row.id, from, tests, false);
    }
    // Job 5's tests step sampled every 2 s; its baseline, job 2, sampled too, job 1 no longer.
    for (runner, from, to) in [("r5", T0 + 1002, T0 + 1082), ("r2", T0 + 102, T0 + 162)] {
        for at in (from + 2..=to).step_by(2) {
            db.execute(
                "INSERT INTO samples (runner, at, cpu, cpu_usage, memory, network_received, network_sent) \
                 VALUES (?1, ?2, 1000, 0, ?3, 0, 0)",
                params![runner, at * 1000, if runner == "r5" { 3u64 << 30 } else { 2 << 30 }],
            )
            .unwrap();
        }
    }
}

#[tokio::test]
async fn compare_picks_the_baseline_pairs_steps_by_name_and_samples_only_what_is_covered() {
    let dir = TestDir::new("lens-compare");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    compare_history(&path);
    let compared = store.compare(5, None).await.unwrap().unwrap();
    assert!(
        matches!(
            compared.baseline,
            Baseline::Chosen {
                job: 2,
                asked: false,
                ..
            }
        ),
        "the latest earlier success on its branch: {:?}",
        compared.baseline
    );
    let usage = async |job: &api::Job| {
        let series = store
            .job_samples(job.summary.id, std::time::Duration::from_secs(2))
            .await
            .unwrap()
            .unwrap();
        api::StepUsage::of(&series.samples, &job.steps)
    };
    let history = [
        store.job(5).await.unwrap().unwrap(),
        store.job(2).await.unwrap().unwrap(),
    ];
    let usages = [usage(&history[0]).await, usage(&history[1]).await];
    let in_memory = JobComparison::in_history(5, None, &history, |job| {
        usages[usize::from(job.summary.id == 2)].clone()
    });
    assert_eq!(
        in_memory.as_ref(),
        Some(&compared),
        "core compares the history in memory, as the demo does, as the store does"
    );
    let tests = &compared.steps[1];
    assert_eq!(
        (tests.matched, tests.time.change_words()),
        (StepMatch::Matched, "+20s (+33%)".to_owned())
    );
    assert_eq!(
        tests
            .memory
            .map(|memory| (memory.sampled, memory.this, memory.baseline)),
        Some((true, Some(3 << 30), Some(2 << 30)))
    );
    assert_eq!(
        compared.steps[2..4]
            .iter()
            .map(|step| step.matched)
            .collect::<Vec<_>>(),
        [StepMatch::Ambiguous; 2]
    );
    assert!(
        compared
            .figures
            .iter()
            .any(|figure| figure.figure == FigureKind::Run && figure.change_words() == "same"),
        "{:?}",
        compared.figures
    );

    // Job 1, past sample retention: its step times compare, its steps' memory is not guessed.
    let asked = store.compare(5, Some(1)).await.unwrap().unwrap();
    assert!(matches!(
        asked.baseline,
        Baseline::Chosen {
            job: 1,
            asked: true,
            ..
        }
    ));
    assert_eq!(asked.steps[1].memory, None);
    assert_eq!(asked.steps[1].time.baseline, Some(60));

    let other = store.compare(5, Some(6)).await.unwrap().unwrap();
    assert_eq!(other.baseline, Baseline::Incompatible { job: 6 });
    assert_eq!(store.compare(5, Some(99)).await.unwrap(), None);
    assert_eq!(store.compare(99, None).await.unwrap(), None);
    let first = store.compare(1, None).await.unwrap().unwrap();
    assert_eq!(
        first.baseline,
        Baseline::Unavailable {
            reason: api::Unavailable::NoEarlierSuccess
        }
    );
    assert_eq!(
        JobComparison::baseline_of(
            &store.job(5).await.unwrap().unwrap().summary,
            &every_job(&store).await
        )
        .unwrap()
        .id,
        2,
        "core picks the same baseline from the same history"
    );
}

#[tokio::test]
async fn a_v21_history_is_searched_sorted_marked_and_laid_out_once_opened() {
    let dir = TestDir::new("lens-21");
    let path = dir.0.join("runnerd.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(SCHEMA_21).unwrap();
    for row in [
        slot_job(1, "jvm", 1, T0, Some(T0 + 100)),
        slot_job(2, "jvm", 1, T0 + 200, Some(T0 + 250)),
    ] {
        insert(&db, &row);
        steps(&db, row.id, row.started_at.unwrap() / 1000, 30, row.id == 2);
    }
    db.execute(
        "INSERT INTO records (job, created_at, bytes) VALUES (1, 1000, 10)",
        [],
    )
    .unwrap();
    db.pragma_update(None, "user_version", 21).unwrap();
    drop(db);
    let store = Store::open(&path).unwrap();
    assert_eq!(user_version(&path), VERSION);
    let list = store
        .jobs(JobQuery {
            q: Some("BUILD".to_owned()),
            sort: Some(JobSort::Run),
            record: Some(RecordFilter::Kept),
            ..JobQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        list.page
            .jobs
            .iter()
            .map(|job| (job.id, job.record_mark))
            .collect::<Vec<_>>(),
        [(1, Some(RecordMark::Kept))]
    );
    let timeline = store
        .timeline(Span {
            from: T0,
            to: T0 + 300,
        })
        .await
        .unwrap();
    assert_eq!(timeline.lanes[0].bars.len(), 2);
    let compared = store.compare(2, None).await.unwrap().unwrap();
    assert!(matches!(compared.baseline, Baseline::Chosen { job: 1, .. }));
    assert_eq!(
        compared
            .steps
            .iter()
            .filter(|step| matches!(step.matched, StepMatch::OnlyThis | StepMatch::OnlyBaseline))
            .count(),
        2,
        "a renamed step pairs with nothing"
    );
}
