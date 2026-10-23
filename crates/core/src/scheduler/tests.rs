//! `plan` as a pure function: slots, labels, the budget, idle runners, paused and held repositories.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use super::{Decision, IDLE_TIMEOUT, Profile, Repository, RunnerSlot, Slot, SlotRef, plan};
use crate::github::QueuedJob;

const GIB: u64 = 1 << 30;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
}

fn job(id: u64, labels: &[&str]) -> QueuedJob {
    QueuedJob {
        id,
        run_id: id,
        name: format!("job {id}"),
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        ..QueuedJob::default()
    }
}

fn repository(name: &str, profile: &str, queued: Vec<QueuedJob>) -> Repository {
    Repository {
        name: name.to_owned(),
        profile: profile.to_owned(),
        labels: vec![
            "self-hosted".to_owned(),
            "linux".to_owned(),
            profile.to_owned(),
        ],
        paused: false,
        held_until: None,
        queued,
        runner_memory: None,
        runner_cpu: None,
    }
}

fn profiles(slots: Vec<Slot>) -> BTreeMap<String, Profile> {
    BTreeMap::from([(
        "app".to_owned(),
        Profile {
            memory: 4 * GIB,
            cpus: 2000,
            preparing: false,
            slots,
        },
    )])
}

fn idle_runner(repository: &str, started_at: SystemTime) -> Slot {
    Slot::Runner(RunnerSlot {
        repository: repository.to_owned(),
        busy: false,
        started_at,
    })
}

fn starts(decisions: &[Decision]) -> Vec<(usize, &str)> {
    decisions
        .iter()
        .filter_map(|decision| match decision {
            Decision::Start {
                slot, repository, ..
            } => Some((slot.index, repository.as_str())),
            Decision::Stop { .. } | Decision::HeldForCpu { .. } => None,
        })
        .collect()
}

#[test]
fn two_repositories_sharing_two_slots_start_two_of_three_jobs_oldest_first() {
    let repositories = [
        repository("o/a", "app", vec![job(3, &["self-hosted", "app"])]),
        repository(
            "o/b",
            "app",
            vec![job(1, &["self-hosted", "app"]), job(2, &["app"])],
        ),
    ];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        None,
        None,
        now(),
    );
    assert_eq!(starts(&decisions), [(0, "o/b"), (1, "o/b")]);
}

#[test]
fn a_job_whose_labels_match_no_repository_never_starts() {
    let repositories = [repository(
        "o/a",
        "app",
        vec![job(1, &["ubuntu-latest"]), job(2, &["self-hosted", "gpu"])],
    )];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free]),
        None,
        None,
        now(),
    );
    assert_eq!(decisions, []);
}

#[test]
fn labels_match_regardless_of_case() {
    let repositories = [repository(
        "o/a",
        "app",
        vec![job(1, &["Self-Hosted", "APP"])],
    )];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free]),
        None,
        None,
        now(),
    );
    assert_eq!(starts(&decisions), [(0, "o/a")]);
}

#[test]
fn a_full_budget_starts_nothing() {
    let repositories = [repository("o/a", "app", vec![job(1, &["app"])])];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free]),
        Some(4 * GIB - 1),
        None,
        now(),
    );
    assert_eq!(decisions, []);
}

#[test]
fn the_budget_caps_starts_within_one_plan() {
    let repositories = [repository(
        "o/a",
        "app",
        vec![job(1, &["app"]), job(2, &["app"])],
    )];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        Some(5 * GIB),
        None,
        now(),
    );
    assert_eq!(starts(&decisions), [(0, "o/a")]);
}

#[test]
fn runners_that_reserve_their_predicted_peak_start_more_within_the_same_budget() {
    // 4G caps fit one in 5G; 2G predictions fit two, and each start says what it reserved.
    let repositories = [Repository {
        runner_memory: Some(2 * GIB),
        ..repository(
            "o/a",
            "app",
            vec![job(1, &["app"]), job(2, &["app"]), job(3, &["app"])],
        )
    }];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free, Slot::Free]),
        Some(5 * GIB),
        None,
        now(),
    );
    assert_eq!(starts(&decisions), [(0, "o/a"), (1, "o/a")]);
    assert!(
        decisions.iter().all(
            |decision| matches!(decision, Decision::Start { memory, .. } if *memory == 2 * GIB)
        ),
        "{decisions:?}"
    );
}

#[test]
fn a_runner_still_waiting_for_its_job_is_not_started_twice() {
    let repositories = [repository("o/a", "app", vec![job(1, &["app"])])];
    let decisions = plan(
        &repositories,
        &profiles(vec![idle_runner("o/a", now()), Slot::Free]),
        None,
        None,
        now(),
    );
    assert_eq!(decisions, []);
}

#[test]
fn an_idle_runner_stops_only_after_its_timeout_with_nothing_queued() {
    let profiles_with = |started_at| profiles(vec![idle_runner("o/a", started_at)]);
    let nothing_queued = [repository("o/a", "app", Vec::new())];
    let just_started = now() - IDLE_TIMEOUT + Duration::from_secs(1);
    assert_eq!(
        plan(
            &nothing_queued,
            &profiles_with(just_started),
            None,
            None,
            now()
        ),
        []
    );
    let long_ago = now() - IDLE_TIMEOUT;
    let stop = Decision::Stop {
        slot: SlotRef {
            profile: "app".to_owned(),
            index: 0,
        },
    };
    assert_eq!(
        plan(&nothing_queued, &profiles_with(long_ago), None, None, now()),
        [stop]
    );
    let still_queued = [repository("o/a", "app", vec![job(1, &["app"])])];
    assert_eq!(
        plan(&still_queued, &profiles_with(long_ago), None, None, now()),
        []
    );
}

#[test]
fn paused_held_and_reseeding_start_nothing() {
    let queued = || vec![job(1, &["app"])];
    let mut paused = repository("o/a", "app", queued());
    paused.paused = true;
    let mut held = repository("o/a", "app", queued());
    held.held_until = Some(now() + Duration::from_secs(1));
    for repositories in [[paused], [held]] {
        assert_eq!(
            plan(
                &repositories,
                &profiles(vec![Slot::Free]),
                None,
                None,
                now()
            ),
            []
        );
    }
    let mut reseeding = profiles(vec![Slot::Free]);
    reseeding.get_mut("app").unwrap().preparing = true;
    let repositories = [repository("o/a", "app", queued())];
    assert_eq!(plan(&repositories, &reseeding, None, None, now()), []);
}

#[test]
fn a_job_with_a_slot_and_memory_but_no_cpu_waits_and_its_repository_is_held_for_cpu() {
    // Each runner reserves 1.5 CPUs of 4: two start, the third waits, while a lighter job of o/b passes it.
    let repositories = [
        Repository {
            runner_cpu: Some(1500),
            ..repository(
                "o/a",
                "app",
                vec![job(1, &["app"]), job(2, &["app"]), job(3, &["app"])],
            )
        },
        Repository {
            runner_cpu: Some(500),
            ..repository("o/b", "app", vec![job(4, &["app"])])
        },
    ];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free; 4]),
        None,
        Some(4000),
        now(),
    );
    assert_eq!(starts(&decisions), [(0, "o/a"), (1, "o/a"), (2, "o/b")]);
    assert!(
        decisions.contains(&Decision::HeldForCpu {
            repository: "o/a".to_owned()
        }),
        "o/a is held for CPU"
    );
}

#[test]
fn a_runner_at_its_profiles_cpus_that_do_not_fit_holds_its_repository_once() {
    let repositories = [repository(
        "o/a",
        "app",
        vec![job(1, &["app"]), job(2, &["app"])],
    )];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        None,
        Some(1000),
        now(),
    );
    // A runner reserves the slot's 2 CPUs by default, which do not fit 1: one hold, not one per job.
    assert_eq!(
        decisions,
        [Decision::HeldForCpu {
            repository: "o/a".to_owned()
        }]
    );
}
