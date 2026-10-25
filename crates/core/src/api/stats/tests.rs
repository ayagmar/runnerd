//! What every client says of the history's statistics.

use crate::api::{Bucket, Stats, StatsRow, StatsSummary};

fn row(start: u64, repository: &str, jobs: u64, succeeded: u64, memory_peak: u64) -> StatsRow {
    StatsRow {
        start,
        repository: repository.to_owned(),
        jobs,
        succeeded,
        failed: jobs - succeeded,
        cancelled: 0,
        queue_seconds: jobs * 10,
        run_seconds: jobs * 60,
        cpu_time: 0,
        memory_peak,
        network_received: 0,
        mirror_bytes: 0,
    }
}

#[test]
fn days_sum_every_repository_and_keep_empty_days() {
    let stats = Stats {
        bucket: Bucket::Day,
        rows: vec![
            row(0, "o/a", 3, 3, 2 << 30),
            row(0, "o/b", 1, 0, 0),
            row(2 * 86_400, "o/a", 2, 1, 0),
        ],
    };
    let days = stats.daily(0, 3);
    assert_eq!(days.len(), 3, "the empty second day is there");
    assert_eq!((days[0].jobs, days[0].succeeded), (4, 3));
    assert_eq!(days[0].success_percent(), Some(75));
    assert_eq!(days[0].average_run(), Some(60));
    assert_eq!(days[1].success_percent(), None, "no jobs, no share");
    assert_eq!(days[2].memory_peak, None, "no sample is no peak, not 0B");
    let total = StatsSummary::of(&days);
    assert_eq!((total.jobs, total.succeeded, total.failed), (6, 4, 2));
    assert_eq!(total.memory_peak, Some(2 << 30));
    assert_eq!(total.average_wait(), Some(10));
}
