//! Schema 16: a history at 15 brought up to it, or read as it by a read-only look, and waits that leave out the
//! time no runnerd ran.

use rusqlite::Connection;

use super::{keep_since, queued, user_version};
use crate::api::{Bucket, HostSample, StatsQuery};
use crate::config::Config;
use crate::github::QueuedJob;
use crate::store::{SCHEMA, Store, VERSION};
use crate::testdir::TestDir;

/// The schema at version 15, as runnerd 0.0.19 to 0.0.22 created it.
const SCHEMA_15: &str = include_str!("schema-15.sql");

const SECOND: u64 = 1000;
const HOUR: u64 = 3600 * SECOND;

/// A history at version 15 at `path`, holding every job since 1970, with one job that ran: queued at 1000 s,
/// started 30 s later, finished at 1100 s, peaking at 1G.
fn history_15(path: &std::path::Path) {
    let db = Connection::open(path).unwrap();
    db.execute_batch(SCHEMA_15).unwrap();
    db.execute_batch(
        "UPDATE history_kept SET jobs_since = 0;
         INSERT INTO runners (name, repository, profile, slot, container, started_at, removed_at, memory_cap)
         VALUES ('r', 'o/r', 'app', 1, 'c', 1000000, 1100000, 4294967296);
         INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, conclusion, runner, queued_at,
         started_at, finished_at, memory_peak)
         VALUES (1, 'o/r', 10, 1, 'build', '[]', 'completed', 'success', 'r', 1000000, 1030000, 1100000, 1073741824);",
    )
    .unwrap();
    db.pragma_update(None, "user_version", 15).unwrap();
}

#[tokio::test]
async fn a_history_at_15_reads_read_only_as_it_is_and_opens_at_16_with_its_rows_and_their_waits() {
    let dir = TestDir::new("store-15");
    let path = dir.0.join("runnerd.db");
    history_15(&path);
    // `runnerd doctor` and `simulate` read it before the daemon restarts, and leave it at 15.
    let history = super::super::sizing_at(&path, 0).unwrap().unwrap();
    let wait = history.jobs.first().and_then(crate::sizing::Ran::wait);
    assert_eq!(
        wait.map(|wait| wait.counted(history.kept_since)),
        Some(30 * SECOND)
    );
    assert_eq!(user_version(&path), 15);
    let store = Store::open(&path).unwrap();
    assert_eq!(user_version(&path), VERSION);
    let job = store.job(1).await.unwrap().unwrap().summary;
    assert_eq!((job.timeline.unseen, job.waited(0)), (0, Some(30)));
    let activity = store.activity(1000, 1100).await.unwrap();
    assert_eq!(activity.wait_p95(), Some(30), "an old row keeps its wait");
}

/// Each table's columns and each index, as SQLite describes them.
fn described(db: &Connection) -> Vec<String> {
    let mut tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    tables.retain(|table| table != "sqlite_sequence");
    let mut described = Vec::new();
    for table in tables {
        let columns = db
            .prepare(
                "SELECT cid, name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1) ORDER BY cid",
            )
            .unwrap()
            .query_map([&table], |row| {
                Ok(format!(
                    "{table}: {} {} {} not null {} default {:?} pk {}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        described.extend(columns);
    }
    let indexes = db
        .prepare("SELECT tbl_name, name, sql FROM sqlite_master WHERE type = 'index' ORDER BY name")
        .unwrap()
        .query_map([], |row| {
            Ok(format!(
                "index {}.{}: {:?}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    described.extend(indexes);
    described
}

#[test]
fn a_history_brought_up_from_15_has_the_schema_a_new_one_gets() {
    let dir = TestDir::new("store-drift");
    let old = dir.0.join("old.db");
    history_15(&old);
    drop(Store::open(&old).unwrap());
    let new = Connection::open_in_memory().unwrap();
    new.execute_batch(SCHEMA).unwrap();
    // A schema.sql change without its migration, or the other way round, differs here.
    assert_eq!(described(&Connection::open(&old).unwrap()), described(&new));
}

/// `at` (ms) as GitHub writes a time.
fn rfc3339(at: u64) -> String {
    Connection::open_in_memory()
        .unwrap()
        .query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch')",
            [at / SECOND],
            |row| row.get(0),
        )
        .unwrap()
}

const CONFIG: &str = r#"
[host]
slice = "runnerd.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.app]
image_stage = "node"
cpus = 4
memory = "4G"
slots = 2

[[repositories]]
name = "o/r"
profile = "app"
labels = ["self-hosted"]
"#;

#[tokio::test]
async fn waits_leave_out_the_time_no_runnerd_ran_wherever_they_are_counted() {
    let dir = TestDir::new("store-downtime");
    let path = dir.0.join("runnerd.db");
    // From a whole second at the history's start, so every wait is whole seconds.
    let start = crate::time::now_ms() / SECOND * SECOND + SECOND;
    let queue = |store: &Store, id: u64, at: u64| {
        store.queued(
            "o/r",
            &QueuedJob {
                created_at: at,
                ..queued(id)
            },
        );
    };
    // The first daemon sees jobs 1 and 5, and stops 20 s after it started.
    let first = Store::open_at(&path, start).unwrap();
    keep_since(&path, start);
    queue(&first, 1, start + 10 * SECOND);
    queue(&first, 5, start + 5 * SECOND);
    first.host_sample(HostSample {
        at: (start + 20 * SECOND) / SECOND,
        cpu: 0,
        memory: 0,
    });
    first.flush().await;
    drop(first);
    // Eight hours later the next one sees job 2, queued before the history's start, and job 3, queued while none
    // ran; job 4 it sees at once.
    let restart = start + 8 * HOUR;
    let second = Store::open_at(&path, restart).unwrap();
    queue(&second, 2, start - HOUR);
    queue(&second, 3, start + HOUR);
    queue(&second, 4, restart + 60 * SECOND);
    let started = restart + 100 * SECOND;
    for job in 1..=5 {
        second.claimed(job, 10, "r", started);
    }
    // A runner that outlived the first daemon took job 5 two hours into the downtime, as GitHub then says.
    second.details(crate::github::JobDetails {
        id: 5,
        run_id: 10,
        run_attempt: 1,
        name: "job 5".to_owned(),
        workflow_name: None,
        head_branch: None,
        html_url: None,
        labels: Vec::new(),
        created_at: None,
        status: "in_progress".to_owned(),
        conclusion: None,
        started_at: Some(rfc3339(start + 2 * HOUR)),
        completed_at: None,
        runner_name: Some("r".to_owned()),
        steps: Vec::new(),
    });
    for job in 1..=5 {
        second.lost(job, started + 60 * SECOND);
    }
    second.flush().await;
    // Job 1 waited 10 s before the first daemon stopped and 100 s after the second started; job 2 the first
    // daemon's 20 s and the 100 s; job 3 and job 4 the time from the second's start and from their queueing; job 5
    // the 15 s before the first daemon stopped.
    let waits = [110, 120, 100, 40, 15];
    let now = (started + HOUR) / SECOND;
    let activity = second.activity(0, now).await.unwrap();
    let mut counted: Vec<u64> = activity.started.iter().map(|job| job.waited).collect();
    counted.sort_unstable();
    let mut expected = waits.to_vec();
    expected.sort_unstable();
    assert_eq!(counted, expected, "the dashboard's waits");
    let config = Config::parse(CONFIG).unwrap();
    let history = second.insights(0).await.unwrap();
    let insights = crate::insights::insights(&history, &config, 14, now * SECOND);
    let week = &insights.waits.first().unwrap().now.all;
    assert_eq!(
        (week.jobs, week.p95),
        (5, activity.wait_p95()),
        "insights' p95 is the dashboard's"
    );
    let stats = second
        .stats(
            StatsQuery {
                repository: None,
                since: Some(0),
                until: None,
                bucket: Bucket::Day,
            },
            now,
        )
        .await
        .unwrap();
    let queued: u64 = stats.rows.iter().map(|row| row.queue_seconds).sum();
    assert_eq!(queued, waits.iter().sum::<u64>(), "the stats' summed waits");
    // Its own page keeps its whole wait, and says how much of it passed while none ran.
    let job = second.job(3).await.unwrap().unwrap().summary;
    assert_eq!(job.waited(now), Some(7 * 3600 + 100));
    assert_eq!(
        job.unseen_note().as_deref(),
        Some("7h 00m of it while runnerd was not running")
    );
    let seen = second.job(4).await.unwrap().unwrap().summary;
    assert_eq!((seen.waited(now), seen.unseen_note()), (Some(40), None));
}
