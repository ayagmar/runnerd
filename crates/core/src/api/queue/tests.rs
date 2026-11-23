//! Whether queued jobs wait for the host's memory, as a local build's wrapper asks it of `runnerctl status`.

use crate::api::{Budget, HostWait, QueueShown, QueuedJob, RepositoryStatus, Status, Waiting};

const LEDGER: &str = "halyard/ledger";
const STOREFRONT: &str = "halyard/storefront";

fn status(queued: Vec<QueuedJob>) -> Status {
    Status {
        job_actions: crate::api::JobActions::On,
        version: "0.0.0".to_owned(),
        shutting_down: false,
        stopping: None,
        github_limited_until: None,
        action_archives: None,
        machine: None,
        budget: Budget::default(),
        profiles: Vec::new(),
        repositories: [LEDGER, STOREFRONT]
            .map(|name| RepositoryStatus {
                name: name.to_owned(),
                pools: Vec::new(),
                paused: false,
                queued_jobs: 0,
                rerun_refused: None,
                listing: None,
                queue: QueueShown::Listed,
            })
            .into(),
        queued,
    }
}

fn queued(id: u64, repository: &str, waiting: impl Into<Option<Waiting>>) -> QueuedJob {
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
        queued_at: id,
        waiting: waiting.into(),
    }
}

/// `status` with `repository`'s queue shown as `queue`.
fn shown(mut status: Status, repository: &str, queue: QueueShown) -> Status {
    for served in &mut status.repositories {
        if served.name == repository {
            served.queue = queue;
        }
    }
    status
}

fn host(free: u64) -> Waiting {
    Waiting::HostMemory {
        needs: 8 << 30,
        free,
    }
}

fn behind(job: u64) -> Waiting {
    Waiting::BehindMemory { job }
}

/// The bug: a wrapper asking about its own repository takes memory while that repository's job waits behind
/// another repository's job for the host's memory, which the memory it takes keeps from both.
#[test]
fn a_job_behind_a_job_of_another_repository_waiting_for_the_host_waits_for_the_host_too() {
    let status = status(vec![
        queued(1, STOREFRONT, host(0)),
        queued(2, LEDGER, behind(1)),
    ]);
    assert_eq!(status.host_wait(Some(LEDGER)), HostWait::Waits);
    assert_eq!(status.host_wait(None), HostWait::Waits);
}

/// The bug: a wrapper yields memory to CI for a wait the host's memory does not cause, such as the slice's budget,
/// a slot, or an older job's start.
#[test]
fn waits_the_hosts_memory_does_not_cause_leave_it_clear() {
    let status = status(vec![
        queued(
            1,
            STOREFRONT,
            Waiting::Memory {
                needs: 8 << 30,
                free: 0,
            },
        ),
        queued(2, LEDGER, behind(1)),
        queued(4, LEDGER, Waiting::BehindStart { job: 1 }),
        queued(
            5,
            LEDGER,
            Waiting::NoSlot {
                profile: "jvm".to_owned(),
            },
        ),
        queued(8, STOREFRONT, host(1 << 30)),
    ]);
    assert_eq!(status.host_wait(Some(LEDGER)), HostWait::Clear);
    assert_eq!(status.host_wait(Some(STOREFRONT)), HostWait::Waits);
    assert_eq!(status.host_wait(None), HostWait::Waits);
}

/// The bug: a wrapper takes memory while CI waits for it, because the status left the waiting jobs out: a listing
/// that failed or the rate limit stopped, a daemon that just started or stops, or one too old to say whether it
/// listed. A job it shows waiting is still an answer; every other answer is unavailable, never clear.
#[test]
fn a_queue_the_status_cannot_show_is_unavailable_never_clear() {
    let empty = || status(Vec::new());
    assert_eq!(empty().host_wait(Some(LEDGER)), HostWait::Clear);
    for queue in [QueueShown::Unlisted, QueueShown::Unknown] {
        let status = shown(empty(), LEDGER, queue);
        assert_eq!(
            status.host_wait(Some(LEDGER)),
            HostWait::Unavailable,
            "{queue:?}"
        );
        assert_eq!(status.host_wait(None), HostWait::Unavailable, "{queue:?}");
        assert_eq!(
            status.host_wait(Some(STOREFRONT)),
            HostWait::Clear,
            "{queue:?}: the other repository is listed"
        );
    }
    let stopping = Status {
        shutting_down: true,
        ..empty()
    };
    assert_eq!(stopping.host_wait(Some(LEDGER)), HostWait::Unavailable);

    let waiting = shown(
        status(vec![queued(1, STOREFRONT, host(0))]),
        LEDGER,
        QueueShown::Unlisted,
    );
    assert_eq!(
        waiting.host_wait(None),
        HostWait::Waits,
        "a job seen waiting"
    );

    // A job the status shows without a reason this client reads: not planned yet, behind a job it does not show, or
    // a reason from a newer daemon.
    for job in [
        queued(2, LEDGER, None),
        queued(2, LEDGER, behind(99)),
        queued(2, LEDGER, Waiting::Unknown),
    ] {
        let status = status(vec![job.clone()]);
        assert_eq!(
            status.host_wait(Some(LEDGER)),
            HostWait::Unavailable,
            "{job:?}"
        );
        assert_eq!(
            status.host_wait(Some(STOREFRONT)),
            HostWait::Clear,
            "{job:?}"
        );
    }
}

/// The bug: a status from a daemon that predates `queue` read as listed, so every such answer was clear.
#[cfg(any(feature = "client", feature = "host"))]
#[test]
fn a_status_without_the_queue_field_is_unknown() {
    let mut json = serde_json::to_value(status(Vec::new())).unwrap();
    for served in json["repositories"].as_array_mut().unwrap() {
        served.as_object_mut().unwrap().remove("queue");
    }
    let old: Status = serde_json::from_value(json).unwrap();
    assert_eq!(old.repositories[0].queue, QueueShown::Unknown);
    assert_eq!(old.host_wait(Some(LEDGER)), HostWait::Unavailable);
}

/// The bug: a wrapper whose repository is misspelt, or not configured, reads "nothing waits" forever and never
/// yields. GitHub reads a repository's name in any case, so a different case is the same repository.
#[test]
fn a_repository_the_daemon_does_not_serve_is_unknown_not_clear() {
    let status = status(vec![queued(1, LEDGER, host(0))]);
    assert_eq!(
        status.host_wait(Some("halyard/ledgr")),
        HostWait::UnknownRepository
    );
    assert_eq!(status.host_wait(Some("Halyard/Ledger")), HostWait::Waits);
    assert_eq!(
        status.host_wait(Some(STOREFRONT)),
        HostWait::Clear,
        "a served repository with nothing queued is clear"
    );
}

/// Every queue of three jobs with each reason that matters here, behind-memory links to each job, to itself and to
/// one gone, under every way the status shows each repository's queue and whether the daemon stops. The answer comes
/// back: a cycle in a malformed status does not hang the wrapper. Unfiltered, it waits exactly where a job waits for
/// the host's memory itself, as the attention list's host item reads it; filtered, wherever a job asked about does.
/// It is clear only where every repository asked about is listed, the daemon runs and every job asked about has a
/// reason this client reads.
#[test]
fn every_small_queue_is_answered_and_clear_only_when_it_shows_everything_asked_about() {
    let reasons: Vec<Option<Waiting>> = vec![
        None,
        Some(host(0)),
        Some(Waiting::Memory { needs: 1, free: 0 }),
        Some(Waiting::Unknown),
        Some(behind(1)),
        Some(behind(2)),
        Some(behind(3)),
        Some(behind(4)),
    ];
    let shows = [
        QueueShown::Listed,
        QueueShown::Unlisted,
        QueueShown::Unknown,
    ];
    let repositories = [LEDGER, STOREFRONT, LEDGER];
    let mut answers = 0;
    for a in &reasons {
        for b in &reasons {
            for c in &reasons {
                let jobs: Vec<QueuedJob> = [a, b, c]
                    .into_iter()
                    .zip(repositories)
                    .enumerate()
                    .map(|(index, (waiting, repository))| {
                        queued(index as u64 + 1, repository, waiting.clone())
                    })
                    .collect();
                let asked = |repository: Option<&'static str>| -> Vec<&QueuedJob> {
                    jobs.iter()
                        .filter(|job| repository.is_none_or(|name| job.repository == name))
                        .collect()
                };
                for (ledger, storefront, shutting_down) in shows
                    .iter()
                    .flat_map(|ledger| shows.iter().map(move |storefront| (*ledger, *storefront)))
                    .flat_map(|(ledger, storefront)| {
                        [false, true].map(|stopping| (ledger, storefront, stopping))
                    })
                {
                    let status = Status {
                        shutting_down,
                        ..shown(
                            shown(status(jobs.clone()), LEDGER, ledger),
                            STOREFRONT,
                            storefront,
                        )
                    };
                    for repository in [None, Some(LEDGER), Some(STOREFRONT)] {
                        let answer = status.host_wait(repository);
                        answers += 1;
                        let direct = asked(repository)
                            .into_iter()
                            .any(|job| matches!(job.waiting, Some(Waiting::HostMemory { .. })));
                        if repository.is_none() {
                            assert_eq!(answer == HostWait::Waits, direct, "{jobs:?}");
                        } else if direct {
                            assert_eq!(answer, HostWait::Waits, "{repository:?} in {jobs:?}");
                        }
                        if answer == HostWait::Clear {
                            let listed = match repository {
                                None => {
                                    ledger == QueueShown::Listed && storefront == QueueShown::Listed
                                }
                                Some(LEDGER) => ledger == QueueShown::Listed,
                                Some(_) => storefront == QueueShown::Listed,
                            };
                            assert!(
                                listed && !shutting_down,
                                "clear only when listed: {repository:?} {ledger:?} {storefront:?} {shutting_down}"
                            );
                            assert!(
                                asked(repository).into_iter().all(|job| matches!(
                                    job.waiting,
                                    Some(Waiting::Memory { .. } | Waiting::BehindMemory { .. })
                                )),
                                "clear only with every job's reason read: {repository:?} in {jobs:?}"
                            );
                        }
                    }
                }
            }
        }
    }
    assert_eq!(answers, 8 * 8 * 8 * 3 * 3 * 2 * 3);
}
