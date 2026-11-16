//! What every client says of insights and the digest.

use crate::api::{
    AutoNote, Digest, Insight, InsightKind, Insights, LongWait, RepositoryWaits, Severity, Tone,
    WaitSpread, WeekWaits,
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

/// Each kind's figure, the number its card shows big, with literal values: swapped arguments would put "20 of 13"
/// over a sentence saying 13 of 20, and a test comparing a page with `figure()` itself would pass.
#[test]
fn each_kind_of_insight_shows_the_figure_its_sentence_is_about() {
    const G: u64 = 1 << 30;
    let cases = [
        (
            InsightKind::Flaky {
                flaky: 2,
                total: 31,
            },
            "2 of 31",
            "runs passed on a re-run",
        ),
        (
            InsightKind::NearCap {
                profile: "jvm".to_owned(),
                peak: 81 * G / 10,
                cap: 9 * G,
                auto: AutoNote::Off,
            },
            "8.1G",
            "p95 peak, of jvm's 9G",
        ),
        (
            InsightKind::ColdCache {
                branch: None,
                hits: 3,
                lookups: 40,
            },
            "8%",
            "of 40 lookups hit",
        ),
        (
            InsightKind::Throttled {
                throttled: 4,
                total: 10,
            },
            "4 of 10",
            "runs throttled",
        ),
        (
            InsightKind::Waits {
                before: 90,
                now: 250,
            },
            "4m 10s",
            "p95 wait, from 1m 30s",
        ),
        (
            InsightKind::CpuHeld {
                profile: "jvm".to_owned(),
                cpus: 8,
                held: 13,
                total: 20,
                raised_from: None,
                set: None,
            },
            "13 of 20",
            "runs held back by jvm's 8 CPUs",
        ),
        (
            InsightKind::HostHeld {
                jobs: 1,
                waited: 59,
            },
            "59s",
            "1 job waits for the host's memory",
        ),
        (
            InsightKind::HostHeld {
                jobs: 12,
                waited: 7260,
            },
            "2h 01m",
            "longest of 12 waits for the host's memory",
        ),
        (
            InsightKind::HostLow {
                day: 0,
                seconds: 60,
                lowest: 0,
            },
            "0B",
            "least memory available",
        ),
    ];
    for (kind, value, label) in cases {
        let figure = insight(Severity::Warning, kind.clone()).figure();
        assert_eq!(
            figure
                .as_ref()
                .map(|figure| (figure.value.as_str(), figure.label.as_str())),
            Some((value, label)),
            "{kind:?}"
        );
    }
}
