//! What every client says of insights and the digest.

use crate::api::{Digest, Insight, InsightKind, Insights, LongWait, Severity, Tone, WaitSpread};

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
    };
    assert_eq!(digest.summary_line(), "12 jobs finished, 2 failed");
    assert_eq!(
        digest.waits[0].line(),
        "halyard/api CI / test waited 1m 30s"
    );
}
