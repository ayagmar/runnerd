//! What every client says of insights and the digest.

use crate::api::{
    Digest, Insight, InsightKind, Insights, LongWait, RepositoryWaits, Severity, Tone, WaitSpread,
    WeekWaits,
};

fn insight(severity: Severity, kind: InsightKind) -> Insight {
    Insight {
        key: "k".to_owned(),
        severity,
        repository: "halyard/api".to_owned(),
        workflow: None,
        job: None,
        sentence: "something".to_owned(),
        kind,
        runs: vec![],
    }
}

#[test]
fn a_warning_reads_the_same_whatever_its_kind() {
    let throttled = insight(
        Severity::Warning,
        InsightKind::Throttled {
            throttled: 3,
            total: 10,
        },
    );
    let flaky = insight(Severity::Warning, InsightKind::Flaky { flaky: 2, total: 9 });
    assert_eq!(throttled.tone(), Tone::Warn);
    assert_eq!(flaky.tone(), throttled.tone());
    let notice = insight(
        Severity::Notice,
        InsightKind::Flaky {
            flaky: 1,
            total: 40,
        },
    );
    assert_eq!(notice.tone(), Tone::Neutral, "a notice is not good news");
    let insights = Insights {
        since: 0,
        until: 30 * 86_400,
        insights: vec![throttled, notice, flaky],
        waits: vec![],
        host: vec![],
        disk: None,
    };
    assert_eq!(insights.warning_count(), 2);
    assert_eq!(insights.window_line(), "from the last 30 days");
}

#[test]
fn a_week_without_jobs_has_no_wait_to_show() {
    let none = WaitSpread {
        jobs: 0,
        p50: Some(30),
        p95: Some(60),
    };
    assert_eq!(none.reading(none.p95), None);
    let some = WaitSpread { jobs: 4, ..none };
    assert_eq!(some.reading(some.p95), Some(60));
}

#[test]
fn the_digest_names_each_long_wait_with_its_workflow_and_a_readable_time() {
    let digest = Digest {
        since: 0,
        until: 86_400,
        finished: 12,
        failed: 2,
        new: vec![],
        waits: vec![LongWait {
            job: 1,
            repository: "halyard/api".to_owned(),
            workflow: Some("CI".to_owned()),
            name: "test".to_owned(),
            waited: 90,
            url: None,
        }],
        savings: None,
        host: None,
    };
    assert_eq!(digest.summary_line(), "12 jobs finished, 2 failed");
    assert_eq!(
        digest.waits[0].line(),
        "halyard/api CI / test waited 1m 30s"
    );
}

#[test]
fn a_repository_s_waits_read_as_figures_with_a_dash_where_no_job_took_them() {
    // A week without warm starts shows no warm median rather than a zero, and the week before without jobs no p95.
    let spread = |jobs, p50, p95| WaitSpread { jobs, p50, p95 };
    let waits = RepositoryWaits {
        repository: "halyard/api".to_owned(),
        now: WeekWaits {
            all: spread(20, Some(30), Some(250)),
            warm: spread(0, Some(0), Some(0)),
            cold: spread(20, Some(30), Some(250)),
        },
        before: WeekWaits::default(),
    };
    let shown: Vec<(&str, String)> = waits
        .figures()
        .into_iter()
        .map(|figure| (figure.label, figure.value))
        .collect();
    assert_eq!(
        shown,
        [
            ("Jobs", "20".to_owned()),
            ("Median", "30s".to_owned()),
            ("p95", "4m 10s".to_owned()),
            ("Warm median", "–".to_owned()),
            ("Cold median", "30s".to_owned()),
            ("p95 the week before", "–".to_owned()),
        ]
    );
}
