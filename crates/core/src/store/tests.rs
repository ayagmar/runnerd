//! The history database: migrations, the log cap, and job listing.

use rusqlite::Connection;

use super::{LogPosition, MIGRATIONS, Store, StoreError};
use crate::api::{JobQuery, JobState};
use crate::github::QueuedJob;
use crate::testdir::TestDir;

fn queued(id: u64) -> QueuedJob {
    QueuedJob {
        id,
        run_id: 10,
        name: format!("job {id}"),
        labels: vec!["self-hosted".to_owned()],
        created_at: 1_000_000 + id,
        run_attempt: 1,
        ..QueuedJob::default()
    }
}

#[tokio::test]
async fn a_fresh_database_migrates_to_the_latest_version() {
    let dir = TestDir::new("store");
    let path = dir.0.join("nested/runnerd.db");
    let store = Store::open(&path).unwrap();
    store.queued("o/r", &queued(1));
    store.flush().await;
    let job = store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Queued);
    let version: usize = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, MIGRATIONS.len());
    // Opening it again runs nothing twice.
    drop(store);
    assert!(Store::open(&path).is_ok());
}

#[test]
fn a_database_from_a_newer_runnerd_is_refused() {
    let dir = TestDir::new("store");
    let path = dir.0.join("runnerd.db");
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let error = Store::open(&path).unwrap_err();
    assert!(
        matches!(error, StoreError::Newer { found: 99, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("newer"), "{error}");
}

#[tokio::test]
async fn a_log_stops_at_its_cap_and_says_so() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    store.queued("o/r", &queued(1));
    let position = LogPosition { page: 1, offset: 0 };
    // Each line takes its length plus a newline: 6 bytes.
    store.log(1, vec!["line1".to_owned(); 3], position, 15);
    store.log(1, vec!["line2".to_owned()], position, 15);
    store.flush().await;
    let job = store.job(1).await.unwrap().unwrap();
    assert_eq!((job.summary.log.lines, job.summary.log.bytes), (2, 12));
    assert!(job.summary.log.truncated);
    assert_eq!(store.log_lines(1, 0, 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn jobs_list_newest_first_by_repository_in_pages() {
    let dir = TestDir::new("store");
    let store = Store::open(&dir.0.join("runnerd.db")).unwrap();
    for id in 1..=5 {
        store.queued(if id % 2 == 0 { "o/even" } else { "o/odd" }, &queued(id));
    }
    store.flush().await;
    let page = |before| {
        store.jobs(JobQuery {
            repository: Some("o/odd".to_owned()),
            before,
            limit: Some(2),
            ..JobQuery::default()
        })
    };
    let first = page(None).await.unwrap();
    let ids: Vec<u64> = first.jobs.iter().map(|job| job.id).collect();
    assert_eq!((ids, first.next), (vec![5, 3], Some(3)));
    let second = page(first.next).await.unwrap();
    let ids: Vec<u64> = second.jobs.iter().map(|job| job.id).collect();
    assert_eq!((ids, second.next), (vec![1], None));
}
