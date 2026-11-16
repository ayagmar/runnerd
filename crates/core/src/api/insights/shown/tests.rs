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

/// A card shows an insight's figure big and its sentence under it: a figure that disagreed with the sentence, or a
/// share past 100%, would contradict it.
#[test]
fn an_insights_figure_is_the_number_its_sentence_is_about() {
    let slower = insight(
        Severity::Warning,
        InsightKind::Slower {
            branch: None,
            before: 280,
            now: 370,
            since: None,
        },
    );
    let figure = slower.figure().expect("a figure");
    assert_eq!(
        (figure.value.as_str(), figure.label.as_str()),
        ("6m 10s", "from 4m 40s")
    );
    let cold = insight(
        Severity::Notice,
        InsightKind::ColdCache {
            branch: None,
            hits: 2,
            lookups: 1,
        },
    );
    assert_eq!(
        cold.figure().expect("a figure").value,
        "100%",
        "never past the whole"
    );
    assert_eq!(
        insight(Severity::Notice, InsightKind::Unknown).figure(),
        None
    );
}
