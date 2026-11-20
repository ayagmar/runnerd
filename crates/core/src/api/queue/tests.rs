//! Whether queued jobs wait for the host's memory, as a local build's wrapper asks it of `runnerctl status`.

use crate::api::{Budget, HostWait, QueuedJob, RepositoryStatus, Status, Waiting};

const LEDGER: &str = "halyard/ledger";
const STOREFRONT: &str = "halyard/storefront";

fn status(queued: Vec<QueuedJob>) -> Status {
    Status {
        job_actions: crate::api::JobActions::On,
        version: "0.0.0".to_owned(),
        shutting_down: false,
        stopping: None,
        github_limited_until: None,
        budget: Budget::default(),
        profiles: Vec::new(),
        repositories: [LEDGER, STOREFRONT]
            .map(|name| RepositoryStatus {
                name: name.to_owned(),
                pools: Vec::new(),
                paused: false,
                queued_jobs: 0,
                listing: None,
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
/// a slot, or an older job's start, or behind a job that has since left the queue.
#[test]
fn waits_the_hosts_memory_does_not_cause_leave_it_clear() {
    let status = status(vec![
        queued(
            1,
            STOREFRONT,
            Some(Waiting::Memory {
                needs: 8 << 30,
                free: 0,
            }),
        ),
        queued(2, LEDGER, behind(1)),
        queued(3, LEDGER, behind(99)),
        queued(4, LEDGER, Some(Waiting::BehindStart { job: 1 })),
        queued(
            5,
            LEDGER,
            Some(Waiting::NoSlot {
                profile: "jvm".to_owned(),
            }),
        ),
        queued(6, LEDGER, Some(Waiting::Unknown)),
        queued(7, LEDGER, None),
        queued(8, STOREFRONT, host(1 << 30)),
    ]);
    assert_eq!(status.host_wait(Some(LEDGER)), HostWait::Clear);
    assert_eq!(status.host_wait(Some(STOREFRONT)), HostWait::Waits);
    assert_eq!(status.host_wait(None), HostWait::Waits);
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
/// one gone: the answer comes back (a cycle in a malformed status does not hang the wrapper), every queue is
/// answered as the attention list's host item reads it (a job waits for the host's memory itself), and a repository
/// with a job waiting for the host's memory itself always waits.
#[test]
fn every_small_queue_is_answered_and_agrees_with_the_jobs_waiting_for_the_host_themselves() {
    let reasons: Vec<Option<Waiting>> = vec![
        None,
        Some(host(0)),
        Some(Waiting::Memory { needs: 1, free: 0 }),
        Some(behind(1)),
        Some(behind(2)),
        Some(behind(3)),
        Some(behind(4)),
    ];
    let repositories = [LEDGER, STOREFRONT, LEDGER];
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
                let direct = |repository: Option<&str>| {
                    jobs.iter().any(|job| {
                        repository.is_none_or(|name| job.repository == name)
                            && matches!(job.waiting, Some(Waiting::HostMemory { .. }))
                    })
                };
                let status = status(jobs.clone());
                assert_eq!(
                    status.host_wait(None) == HostWait::Waits,
                    direct(None),
                    "{jobs:?}"
                );
                for repository in [LEDGER, STOREFRONT] {
                    if direct(Some(repository)) {
                        assert_eq!(
                            status.host_wait(Some(repository)),
                            HostWait::Waits,
                            "{repository} in {jobs:?}"
                        );
                    }
                }
            }
        }
    }
}
