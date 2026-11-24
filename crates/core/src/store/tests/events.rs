//! Schema 33: the reads that run every few minutes find events of one type through `events_by_type`, and insights
//! read the host's memory from `host_readings`, kept as each `budget.changed` event is stored and brought up from the
//! events of a history at 32 (ADR 0171).

use rusqlite::Connection;

use super::{DAY_MS, user_version};
use crate::api::{self, Event};
use crate::config::History;
use crate::insights::HostReading;
use crate::store::{Store, VERSION, read};
use crate::testdir::TestDir;

/// `sql`'s plan, one line per step, as `EXPLAIN QUERY PLAN` gives it.
fn plan(db: &Connection, sql: &str) -> Vec<String> {
    let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
    statement
        .query_map(rusqlite::params_from_iter(nulls), |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn the_periodic_reads_of_events_search_an_index_and_never_read_every_event() {
    // Catches a read whose expression drifts from the index's, which SQLite then answers by reading every event: on a
    // month's history, 600 MB each time auto sizing or the attention list looks.
    let dir = TestDir::new("events");
    let path = dir.0.join("runnerd.db");
    drop(Store::open(&path).unwrap());
    let db = Connection::open(&path).unwrap();
    // A month of budget and step events beside a few of the types read, as statistics show them to the planner.
    db.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000) \
         INSERT INTO events (at, body) SELECT i, CASE i % 500 WHEN 0 THEN '{\"type\":\"profile.changed\"}' \
         WHEN 1 THEN '{\"type\":\"cache.reseeded\"}' ELSE '{\"type\":\"budget.changed\"}' END FROM n; ANALYZE;",
    )
    .unwrap();
    for (name, sql) in [
        ("profile changes", read::PROFILE_CHANGES),
        ("reseeds", read::RESEEDS),
        ("host readings", read::HOST_READINGS),
    ] {
        let plan = plan(&db, sql);
        assert!(
            !plan.iter().any(|step| step.starts_with("SCAN events")),
            "{name} reads every event: {plan:?}"
        );
        assert!(
            sql == read::HOST_READINGS || plan.iter().any(|step| step.contains("events_by_type")),
            "{name} searches events by type: {plan:?}"
        );
    }
}

/// A `budget.changed` event of the host's memory: `total` and `available` GiB, swap with 1 GiB free, a 512 MiB
/// credit, and `in_use` bytes counted beside a 1 GiB mirror and a 1 GiB seed.
fn budget(total: u64, available: u64, in_use: u64) -> Event {
    Event::BudgetChanged {
        budget: api::Budget {
            memory_in_use: in_use,
            mirror_memory: 1 << 30,
            seed_memory: vec![api::SeedMemory {
                profile: "node".to_owned(),
                memory: 1 << 30,
            }],
            host: Some(api::HostBudget {
                memory: api::HostMemory {
                    at: 1,
                    total: total << 30,
                    available: available << 30,
                    slice: 0,
                    some: None,
                    full: None,
                    holders: Vec::new(),
                    ceiling: None,
                    swap: Some(api::Swap {
                        total: 4 << 30,
                        free: 1 << 30,
                    }),
                },
                limit: 0,
                spare: 0,
                stalled: false,
                credit: Some(api::HostCredit {
                    bytes: 512 << 20,
                    used: 0,
                }),
            }),
            ..api::Budget::default()
        },
    }
}

/// What insights read of the host's memory, without the times.
async fn readings(store: &Store) -> Vec<HostReading> {
    store
        .insights(0)
        .await
        .unwrap()
        .readings
        .into_iter()
        .map(|reading| HostReading { at: 0, ..reading })
        .collect()
}

#[tokio::test]
async fn host_readings_are_kept_as_budget_events_are_stored_brought_up_from_32_and_pruned_with_events()
 {
    // Catches a reading the writer does not keep, a migration that does not bring up the events a history at 32
    // holds, a reading that differs from the event's, and readings that outlive their events.
    let dir = TestDir::new("events");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    store.event(budget(64, 30, 2 << 30));
    // A budget without a reading of the host is none.
    store.event(Event::BudgetChanged {
        budget: api::Budget::default(),
    });
    store.event(budget(64, 20, 6 << 30));
    store.flush().await;
    let expected = vec![
        HostReading {
            at: 0,
            total: 64 << 30,
            available: 30 << 30,
            swap_free: Some(1 << 30),
            credit: 512 << 20,
            running: false,
        },
        HostReading {
            at: 0,
            total: 64 << 30,
            available: 20 << 30,
            swap_free: Some(1 << 30),
            credit: 512 << 20,
            running: true,
        },
    ];
    assert_eq!(readings(&store).await, expected);
    drop(store);

    // The same events in a history at 32, which kept no readings.
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "DROP TABLE host_readings; DROP TABLE log_packs; DROP INDEX events_by_type; PRAGMA user_version = 32;",
    )
    .unwrap();
    drop(db);
    let store = Store::open(&path).unwrap();
    assert_eq!(user_version(&path), VERSION);
    assert_eq!(readings(&store).await, expected, "brought up from 32");

    let now = crate::time::now_ms();
    store
        .prune(now + 31 * DAY_MS, &History::default())
        .await
        .unwrap();
    assert_eq!(
        readings(&store).await,
        Vec::new(),
        "pruned with their events"
    );
    let left: u64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT count(*) FROM host_readings", [], |row| row.get(0))
        .unwrap();
    assert_eq!(left, 0);
}
