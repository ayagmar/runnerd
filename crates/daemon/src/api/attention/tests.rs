//! When the worker reads the insights again.

use std::time::{Duration, Instant};

use runnerd_core::api::{QueuedJob, Status, Waiting};

use super::{Read, Timing, held};

/// `status` with jobs `ids` waiting for the host's memory, and job 1 for a slot.
fn waiting_for_the_host(status: &Status, ids: &[u64]) -> Status {
    let job = |id: u64, waiting: Waiting| QueuedJob {
        id,
        repository: "halyard/ledger".to_owned(),
        profile: Some("node".to_owned()),
        run_id: 2,
        run_attempt: 1,
        workflow: None,
        name: format!("job {id}"),
        branch: None,
        url: None,
        queued_at: 1_800_000_000,
        waiting: Some(waiting),
    };
    let mut status = status.clone();
    status.queued = ids
        .iter()
        .map(|&id| {
            job(
                id,
                Waiting::HostMemory {
                    needs: 4 << 30,
                    free: 1 << 30,
                },
            )
        })
        .chain([job(
            1,
            Waiting::NoSlot {
                profile: "node".to_owned(),
            },
        )])
        .collect();
    status
}

#[test]
fn insights_read_stand_until_they_age_the_config_is_replaced_or_other_jobs_wait_for_the_host() {
    let timing = Timing {
        every: Duration::from_secs(30),
        insights: Duration::from_mins(1),
    };
    let base = crate::api::tests::status();
    let read_at = |at: Instant| Read {
        at,
        config: "r1".to_owned(),
        held: held(&waiting_for_the_host(&base, &[7, 8])),
        insights: None,
    };
    let read = read_at(Instant::now());
    assert!(
        read.stands("r1", &waiting_for_the_host(&base, &[8, 7]), timing),
        "the same jobs wait, in any order"
    );
    assert!(
        !read.stands("r2", &waiting_for_the_host(&base, &[7, 8]), timing),
        "read with a config no longer running"
    );
    assert!(
        !read.stands("r1", &waiting_for_the_host(&base, &[8, 9]), timing),
        "a job that waited started, and another waits: the host-held insight may name the one gone"
    );
    assert!(
        !read.stands("r1", &waiting_for_the_host(&base, &[]), timing),
        "none waits any more"
    );
    let old = Instant::now()
        .checked_sub(Duration::from_mins(2))
        .unwrap_or_else(Instant::now);
    assert!(
        !read_at(old).stands("r1", &waiting_for_the_host(&base, &[7, 8]), timing),
        "older than a minute"
    );
}
