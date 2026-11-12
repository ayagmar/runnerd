//! What needs attention, from what the daemon serves: each source only on evidence that holds now, in one order, and
//! read by a client older than the daemon that wrote it.

use crate::api::{
    Attention, AttentionItem, AttentionKind, Budget, Check, CheckStatus, Doctor, DoctorState, Fix,
    HoldCause, Insight, InsightKind, Insights, ListingFailure, ListingProblem, QueuedJob,
    RepositoryStatus, RunLink, Severity, Status, Subject, Tone, View, Waiting, attention_items,
};

const NOW: u64 = 1_800_000_000;

fn status() -> Status {
    Status {
        version: "0.0.0".to_owned(),
        shutting_down: false,
        stopping: None,
        github_limited_until: None,
        budget: Budget::default(),
        profiles: Vec::new(),
        repositories: ["halyard/ledger", "halyard/storefront"]
            .map(|name| RepositoryStatus {
                name: name.to_owned(),
                pools: Vec::new(),
                paused: false,
                queued_jobs: 0,
                listing: None,
            })
            .into(),
        queued: Vec::new(),
    }
}

fn queued(id: u64, repository: &str, waiting: Waiting) -> QueuedJob {
    QueuedJob {
        id,
        repository: repository.to_owned(),
        profile: None,
        run_id: id,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: format!("job {id}"),
        branch: Some("main".to_owned()),
        url: None,
        queued_at: NOW - 600 + id,
        waiting: Some(waiting),
    }
}

/// A doctor run for the config the daemon runs with.
fn ready(checks: Vec<Check>) -> Doctor {
    Doctor {
        checks,
        state: Some(DoctorState::Ready { at: NOW - 60 }),
    }
}

fn check(name: &str, status: CheckStatus) -> Check {
    Check {
        name: name.to_owned(),
        status,
        detail: format!("{name} detail"),
        fix: (status == CheckStatus::Fail).then(|| format!("fix {name}")),
    }
}

fn insights(insights: Vec<Insight>) -> Insights {
    Insights {
        since: NOW - 14 * 86_400,
        until: NOW,
        insights,
        waits: Vec::new(),
        host: Vec::new(),
    }
}

fn insight(key: &str, severity: Severity, kind: InsightKind) -> Insight {
    Insight {
        key: key.to_owned(),
        severity,
        repository: "halyard/ledger".to_owned(),
        workflow: Some("CI".to_owned()),
        job: Some("test".to_owned()),
        sentence: format!("{key} sentence"),
        kind,
        runs: vec![RunLink {
            job: 1,
            run_id: 1,
            run_attempt: 1,
            branch: None,
            url: None,
            at: NOW - 3600,
        }],
    }
}

fn flaky(key: &str, severity: Severity) -> Insight {
    insight(key, severity, InsightKind::Flaky { flaky: 2, total: 9 })
}

fn kinds(items: &[AttentionItem]) -> Vec<AttentionKind> {
    items.iter().map(|item| item.kind).collect()
}

fn keys(items: &[AttentionItem]) -> Vec<&str> {
    items.iter().map(|item| item.key.as_str()).collect()
}

#[test]
fn only_a_pause_and_labels_no_pool_serves_keep_runnerd_from_starting_a_job() {
    let backoff = Waiting::Backoff {
        until: NOW + 60,
        cause: HoldCause::Start,
    };
    assert!(Waiting::Paused.cannot_start(), "a pause needs a resume");
    assert!(
        Waiting::NoPool {
            labels: vec!["gpu".to_owned()]
        }
        .cannot_start(),
        "labels no pool serves need a config change"
    );
    for transient in [
        backoff,
        Waiting::Unknown,
        Waiting::RunnerStarting,
        Waiting::NoSlot {
            profile: "jvm".to_owned(),
        },
    ] {
        assert!(
            !transient.cannot_start(),
            "{transient:?} clears by itself or says nothing"
        );
    }
}

#[test]
fn queued_jobs_runnerd_cannot_start_are_one_item_per_repository_and_reason() {
    let mut status = status();
    status.queued = vec![
        queued(1, "halyard/ledger", Waiting::Paused),
        queued(2, "halyard/ledger", Waiting::Paused),
        queued(
            3,
            "halyard/storefront",
            Waiting::NoPool {
                labels: vec!["gpu".to_owned()],
            },
        ),
        queued(
            4,
            "halyard/storefront",
            Waiting::Backoff {
                until: NOW + 30,
                cause: HoldCause::Register,
            },
        ),
    ];
    let items = attention_items(&status, &ready(Vec::new()), None, NOW);
    assert_eq!(
        keys(&items),
        [
            "cannot-start:no-pool:halyard/storefront:gpu",
            "cannot-start:paused:halyard/ledger"
        ],
        "newest evidence first; the backoff is no item"
    );
    let [no_pool, paused] = items.as_slice() else {
        unreachable!()
    };
    assert_eq!(
        paused.title,
        "runnerd cannot start 2 queued jobs of halyard/ledger with the current configuration: halyard/ledger is \
         paused"
    );
    assert_eq!(
        paused.subject,
        Subject::Repository {
            name: "halyard/ledger".to_owned()
        }
    );
    assert_eq!(
        paused.fix.as_ref().map(Fix::line).as_deref(),
        Some("runnerctl resume halyard/ledger")
    );
    assert_eq!(paused.since, Some(NOW - 599), "the oldest job's queue time");
    assert_eq!(
        no_pool.title,
        "runnerd cannot start job 3 of halyard/storefront with the current configuration: no profile of \
         halyard/storefront serves the label gpu"
    );
    assert!(
        matches!(no_pool.subject, Subject::Job { id: 3, .. }),
        "one job is its own subject"
    );
    assert_eq!(
        no_pool.fix.as_ref().map(Fix::line).as_deref(),
        Some("Change repositories.labels in the config")
    );
}

#[test]
fn a_long_wait_for_the_hosts_memory_is_one_item_while_a_job_waits_for_it_now() {
    let held = insight(
        "host-held:1",
        Severity::Warning,
        InsightKind::HostHeld {
            jobs: 1,
            waited: 2400,
        },
    );
    let found = insights(vec![held]);
    let mut status = status();
    // The insight alone, with no job waiting for the host in the status: nothing holds now.
    assert!(
        attention_items(&status, &ready(Vec::new()), Some(&found), NOW).is_empty(),
        "no job waits for the host now"
    );
    status.queued = vec![queued(
        7,
        "halyard/ledger",
        Waiting::HostMemory {
            needs: 1 << 30,
            free: 0,
        },
    )];
    let items = attention_items(&status, &ready(Vec::new()), Some(&found), NOW);
    assert_eq!(
        kinds(&items),
        [AttentionKind::HostMemory],
        "the insight is not listed again"
    );
    assert_eq!(items[0].title, "host-held:1 sentence");
    // The insight was read earlier and names no job: the job waiting now may be another, which has waited seconds.
    assert_eq!(items[0].subject, Subject::Host, "the host, not a job");
    assert_eq!(
        items[0].since, None,
        "no evidence time the insight does not give"
    );
    assert_eq!(
        attention_items(&status, &ready(Vec::new()), Some(&found), NOW + 10),
        items,
        "the same evidence read again later is the same item"
    );
}

#[test]
fn doctor_checks_count_only_from_a_current_run_and_otherwise_say_they_are_not_current() {
    let checks = vec![
        check("Docker", CheckStatus::Pass),
        check("slice", CheckStatus::Warn),
        check("sysbox", CheckStatus::Fail),
        check("repository", CheckStatus::Warn),
        check("repository", CheckStatus::Warn),
        check("kvm", CheckStatus::Info),
    ];
    let items = attention_items(&status(), &ready(checks.clone()), None, NOW);
    assert_eq!(
        keys(&items),
        [
            "doctor:sysbox",
            "doctor:repository:repository detail",
            "doctor:repository:repository detail#2",
            "doctor:slice"
        ],
        "a failure first, passes and notes left out, checks of one name apart by what they found"
    );
    assert_eq!(items[0].tone, Tone::Bad);
    assert_eq!(items[0].title, "sysbox: sysbox detail");
    assert_eq!(
        items[0].fix,
        Some(Fix::Text {
            text: "fix sysbox".to_owned()
        })
    );
    assert_eq!(items[0].since, Some(NOW - 60), "when the run finished");
    for (state, tone) in [
        (DoctorState::NotRun, Tone::Quiet),
        (DoctorState::Stale { at: NOW - 7200 }, Tone::Quiet),
        (
            DoctorState::Failed {
                at: NOW - 5,
                error: "it took longer than 60s".to_owned(),
            },
            Tone::Warn,
        ),
        (DoctorState::Unknown, Tone::Quiet),
    ] {
        let doctor = Doctor {
            checks: checks.clone(),
            state: Some(state.clone()),
        };
        let items = attention_items(&status(), &doctor, None, NOW);
        assert_eq!(
            kinds(&items),
            [AttentionKind::DoctorUnavailable],
            "{state:?}: one item, never the old warnings, never an all clear"
        );
        assert_eq!(items[0].tone, tone, "{state:?}");
        assert_eq!(Some(items[0].title.clone()), state.line(NOW));
        let passing = Doctor {
            state: Some(state.clone()),
            ..ready(vec![check("Docker", CheckStatus::Pass)])
        };
        assert!(
            !passing.passed(),
            "{state:?}: checks not current never pass"
        );
    }
    assert!(
        ready(vec![check("Docker", CheckStatus::Pass)]).passed(),
        "a current run with no failure passes"
    );
}

#[test]
fn warning_insights_that_hold_now_are_items_and_notices_and_past_days_are_not() {
    let found = insights(vec![
        flaky("flaky:halyard/ledger/CI/test", Severity::Warning),
        flaky("flaky:halyard/ledger/CI/lint", Severity::Notice),
        insight(
            "host-low:1",
            Severity::Warning,
            InsightKind::HostLow {
                day: NOW - 86_400,
                seconds: 60,
                lowest: 1 << 30,
            },
        ),
    ]);
    let items = attention_items(&status(), &ready(Vec::new()), Some(&found), NOW);
    assert_eq!(keys(&items), ["insight:flaky:halyard/ledger/CI/test"]);
    assert_eq!(
        items[0].fix,
        Some(Fix::Open {
            view: View::Insights
        })
    );
    assert_eq!(items[0].since, Some(NOW - 3600), "its newest run");
}

#[test]
fn a_refused_token_is_one_item_for_every_repository_and_other_failures_one_each() {
    let mut status = status();
    status.repositories[0].listing = Some(ListingFailure {
        since: NOW - 300,
        problem: ListingProblem::Unauthorized,
    });
    status.repositories[1].listing = Some(ListingFailure {
        since: NOW - 200,
        problem: ListingProblem::Unauthorized,
    });
    status.github_limited_until = Some(NOW + 600);
    let items = attention_items(&status, &ready(Vec::new()), None, NOW);
    assert_eq!(
        kinds(&items),
        [AttentionKind::GithubAuth, AttentionKind::RateLimited]
    );
    assert_eq!(
        kinds(&attention_items(
            &status,
            &ready(Vec::new()),
            None,
            NOW + 600
        )),
        [AttentionKind::GithubAuth],
        "the rate limit reset at its time, with no new status"
    );
    assert_eq!(items[0].since, Some(NOW - 300), "the earliest failure");
    assert_eq!(
        items[0].title,
        "GitHub refuses runnerd's token (401), so it lists no repository's queued jobs"
    );

    status.repositories[0].listing = Some(ListingFailure {
        since: NOW - 300,
        problem: ListingProblem::Refused { status: 404 },
    });
    status.repositories[1].listing = Some(ListingFailure {
        since: NOW - 100,
        problem: ListingProblem::Failed { status: 502 },
    });
    status.github_limited_until = None;
    let items = attention_items(&status, &ready(Vec::new()), None, NOW);
    assert_eq!(
        keys(&items),
        [
            "github-listing:halyard/storefront",
            "github-listing:halyard/ledger"
        ]
    );
    assert_eq!(
        items[1].title,
        "Cannot list halyard/ledger's queued jobs: GitHub answers 404; the token cannot see the repository, or it \
         is gone"
    );
    assert_eq!(
        items[0].title,
        "Cannot list halyard/storefront's queued jobs: GitHub answers 502"
    );
}

#[test]
fn items_go_by_tone_then_newest_evidence_then_key() {
    let mut status = status();
    status.github_limited_until = Some(NOW + 60);
    let found = insights(vec![
        flaky("b", Severity::Warning),
        flaky("a", Severity::Warning),
    ]);
    let doctor = ready(vec![check("sysbox", CheckStatus::Fail)]);
    let items = attention_items(&status, &doctor, Some(&found), NOW);
    assert_eq!(
        keys(&items),
        ["doctor:sysbox", "insight:a", "insight:b", "rate-limited"],
        "bad first; warnings by evidence time, the unknown last; ties by key"
    );
}

#[test]
fn many_items_are_capped_and_the_rest_counted_in_core_s_words() {
    let checks: Vec<_> = (0..60)
        .map(|n| check(&format!("check {n:02}"), CheckStatus::Warn))
        .collect();
    let attention = Attention::new(
        "a1".to_owned(),
        7,
        attention_items(&status(), &ready(checks), None, NOW),
    );
    assert_eq!(attention.items.len(), crate::api::ATTENTION_MAX);
    assert_eq!(attention.omitted, 10);
    assert_eq!(attention.more(3).as_deref(), Some("and 57 more"));
    assert_eq!(attention.more(50).as_deref(), Some("and 10 more"));
    let few = Attention::new(
        "a1".to_owned(),
        8,
        attention_items(&status(), &ready(Vec::new()), None, NOW),
    );
    assert_eq!(few.more(3), None, "nothing left out");
}

#[test]
fn the_same_items_are_the_same_content_at_any_revision_and_one_replaced_is_not() {
    let one = ready(vec![check("slice", CheckStatus::Warn)]);
    let other = ready(vec![check("sysbox", CheckStatus::Warn)]);
    let first = Attention::new(
        "a1".to_owned(),
        1,
        attention_items(&status(), &one, None, NOW),
    );
    assert!(first.same_content(&Attention::new(
        "a2".to_owned(),
        2,
        attention_items(&status(), &one, None, NOW)
    )));
    let replaced = Attention::new(
        "a1".to_owned(),
        2,
        attention_items(&status(), &other, None, NOW),
    );
    assert_eq!(replaced.items.len(), first.items.len());
    assert!(
        !first.same_content(&replaced),
        "one item replaced by another at the same count is new content"
    );
}

#[test]
fn a_client_reads_items_of_kinds_subjects_fixes_and_tones_a_newer_daemon_names() {
    let future = r#"{
        "instance": "4f1c-2",
        "revision": 9,
        "items": [
            {"key": "k1", "kind": "disk-full", "tone": "alarm", "title": "Disk is full",
             "subject": {"kind": "volume", "name": "runnerd-pnpm"},
             "fix": {"kind": "open", "view": "timeline"}},
            {"key": "k2", "kind": "doctor", "tone": "bad", "title": "sysbox: down",
             "subject": {"kind": "check", "name": "sysbox"}, "since": 5,
             "fix": {"kind": "wizard", "steps": 3}},
            {"key": "k3", "kind": "insight", "tone": "warn", "title": "slower",
             "subject": {"kind": "insight", "key": "slower:x"},
             "fix": {"kind": "open", "view": "insights"}}
        ],
        "omitted": 4,
        "added-later": true
    }"#;
    let read: Attention = serde_json::from_str(future).unwrap();
    assert_eq!(read.revision, 9);
    assert_eq!(read.omitted, 4);
    let [unknown, doctor, insight] = read.items.as_slice() else {
        panic!("three items: {read:?}")
    };
    assert_eq!(unknown.kind, AttentionKind::Unknown);
    assert_eq!(
        unknown.tone,
        Tone::Neutral,
        "an unknown tone is drawn neutral"
    );
    assert_eq!(unknown.subject, Subject::Unknown);
    assert_eq!(unknown.fix, None, "a view this client lacks is no fix");
    assert_eq!(unknown.title, "Disk is full", "still shown by its title");
    assert_eq!(doctor.fix, None, "an unknown fix is none");
    assert_eq!(doctor.since, Some(5));
    assert_eq!(
        insight.fix,
        Some(Fix::Open {
            view: View::Insights
        })
    );
    // What this version writes, it reads back whole.
    let written = serde_json::to_string(&read).unwrap();
    assert_eq!(serde_json::from_str::<Attention>(&written).unwrap(), read);
}
