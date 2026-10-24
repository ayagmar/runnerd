//! `plan` as a pure function: slots, labels, the budget, idle runners, paused and held repositories.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use super::{
    Budget, CPU_PRIORITY_AFTER, Decision, Hold, IDLE_TIMEOUT, Plan, Preparing, Profile, Repository,
    RunnerSlot, Slot, SlotRef, plan,
};
use crate::api::{HoldCause, Waiting};
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
        held: None,
        queued,
        runner_memory: None,
        runner_cpu: None,
        cpu_waiting_since: None,
        warm: 0,
    }
}

fn budget(memory: Option<u64>, cpu: Option<u64>) -> Budget {
    Budget {
        memory,
        cpu,
        host: None,
        held: false,
    }
}

fn profiles(slots: Vec<Slot>) -> BTreeMap<String, Profile> {
    BTreeMap::from([(
        "app".to_owned(),
        Profile {
            memory: 4 * GIB,
            cpus: 2000,
            preparing: None,
            upkeep_due: false,
            count: slots.len(),
            slots,
        },
    )])
}

fn idle_runner(repository: &str, started_at: SystemTime) -> Slot {
    Slot::Runner(RunnerSlot {
        repository: repository.to_owned(),
        busy: false,
        started_at,
        memory: 4 * GIB,
        cpu: 2000,
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
        budget(None, None),
        now(),
    )
    .decisions;
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
        budget(None, None),
        now(),
    )
    .decisions;
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
        budget(None, None),
        now(),
    )
    .decisions;
    assert_eq!(starts(&decisions), [(0, "o/a")]);
}

#[test]
fn a_full_budget_starts_nothing() {
    let repositories = [repository("o/a", "app", vec![job(1, &["app"])])];
    let decisions = plan(
        &repositories,
        &profiles(vec![Slot::Free]),
        budget(Some(4 * GIB - 1), None),
        now(),
    )
    .decisions;
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
        budget(Some(5 * GIB), None),
        now(),
    )
    .decisions;
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
        budget(Some(5 * GIB), None),
        now(),
    )
    .decisions;
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
        budget(None, None),
        now(),
    )
    .decisions;
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
            budget(None, None),
            now()
        )
        .decisions,
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
        plan(
            &nothing_queued,
            &profiles_with(long_ago),
            budget(None, None),
            now()
        )
        .decisions,
        [stop]
    );
    let still_queued = [repository("o/a", "app", vec![job(1, &["app"])])];
    assert_eq!(
        plan(
            &still_queued,
            &profiles_with(long_ago),
            budget(None, None),
            now()
        )
        .decisions,
        []
    );
}

#[test]
fn paused_held_and_reseeding_start_nothing() {
    let queued = || vec![job(1, &["app"])];
    let mut paused = repository("o/a", "app", queued());
    paused.paused = true;
    let mut held = repository("o/a", "app", queued());
    held.held = Some(Hold {
        until: now() + Duration::from_secs(1),
        cause: HoldCause::NoJob,
    });
    for repositories in [[paused], [held]] {
        assert_eq!(
            plan(
                &repositories,
                &profiles(vec![Slot::Free]),
                budget(None, None),
                now()
            )
            .decisions,
            []
        );
    }
    let mut reseeding = profiles(vec![Slot::Free]);
    reseeding.get_mut("app").unwrap().preparing = Some(Preparing::Reseeding);
    let repositories = [repository("o/a", "app", queued())];
    assert_eq!(
        plan(&repositories, &reseeding, budget(None, None), now()).decisions,
        []
    );
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
        budget(None, Some(4000)),
        now(),
    )
    .decisions;
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
        budget(None, Some(1000)),
        now(),
    )
    .decisions;
    // A runner reserves the slot's 2 CPUs by default, which do not fit 1: one hold, not one per job.
    assert_eq!(
        decisions,
        [Decision::HeldForCpu {
            repository: "o/a".to_owned()
        }]
    );
}

/// Ticks 15 s apart against a 4-CPU budget: o/heavy's job, queued first, takes all 4; o/light queues a 1-CPU job
/// every tick, each running a minute. Its runners start staggered, so a CPU frees up every tick. Returns the tick
/// o/heavy's job started at, if it did within `ticks`, and how many light jobs started from the tick it waited
/// `CPU_PRIORITY_AFTER` until then.
fn heavy_job_beside_a_stream_of_light_ones(ticks: u64) -> (Option<u64>, usize) {
    const TICK: Duration = Duration::from_secs(15);
    // Light jobs started at ticks -3 … 0, ending at ticks 1 … 4.
    let mut running: Vec<(&str, u64, u64)> = (1..=4).map(|end| ("o/light", 1000, end)).collect();
    let mut waiting_since = None;
    let mut light_after_limit = 0;
    for tick in 1..=ticks {
        let at = now() + TICK * u32::try_from(tick).unwrap();
        running.retain(|(_, _, end)| *end > tick);
        let in_use: u64 = running.iter().map(|(_, cpu, _)| cpu).sum();
        let slots: Vec<Slot> = (0..10)
            .map(|index| match running.get(index) {
                Some((repository, ..)) => Slot::Runner(RunnerSlot {
                    repository: (*repository).to_owned(),
                    busy: true,
                    started_at: now(),
                    memory: 4 * GIB,
                    cpu: 2000,
                }),
                None => Slot::Free,
            })
            .collect();
        let heavy_queued = !running
            .iter()
            .any(|(repository, ..)| *repository == "o/heavy");
        let repositories = [
            Repository {
                runner_cpu: Some(4000),
                cpu_waiting_since: waiting_since,
                ..repository(
                    "o/heavy",
                    "app",
                    if heavy_queued {
                        vec![job(1, &["app"])]
                    } else {
                        Vec::new()
                    },
                )
            },
            Repository {
                runner_cpu: Some(1000),
                ..repository("o/light", "app", vec![job(1 + tick, &["app"])])
            },
        ];
        let decisions = plan(
            &repositories,
            &profiles(slots),
            budget(None, Some(4000 - in_use)),
            at,
        )
        .decisions;
        for (_, repository) in starts(&decisions) {
            if repository == "o/heavy" {
                return (Some(tick), light_after_limit);
            }
            if waiting_since
                .is_some_and(|since| at.duration_since(since).unwrap() >= CPU_PRIORITY_AFTER)
            {
                light_after_limit += 1;
            }
            running.push(("o/light", 1000, tick + 4));
        }
        let held = decisions.contains(&Decision::HeldForCpu {
            repository: "o/heavy".to_owned(),
        });
        // As the engine marks it: since the first plan that held it.
        waiting_since = if held {
            waiting_since.or(Some(at))
        } else {
            None
        };
    }
    (None, light_after_limit)
}

#[test]
fn a_job_held_for_cpu_long_enough_starts_once_younger_jobs_stop_taking_the_cpu_it_waits_for() {
    let (started, light_after_limit) = heavy_job_beside_a_stream_of_light_ones(40);
    // It waits 120 s from tick 1, then the light runners drain within a minute.
    let started = started.expect("o/heavy's job starts");
    assert!(started <= 1 + 8 + 4, "started at tick {started}");
    assert_eq!(light_after_limit, 0, "no light job started past the limit");
}

#[test]
fn each_job_left_queued_says_why_it_waits() {
    let later = now() + Duration::from_secs(60);
    let repositories = [
        Repository {
            paused: true,
            ..repository("o/paused", "app", vec![job(1, &["app"])])
        },
        Repository {
            held: Some(Hold {
                until: later,
                cause: HoldCause::NoJob,
            }),
            ..repository("o/failing", "app", vec![job(2, &["app"])])
        },
        repository("o/a", "app", vec![job(3, &["app"]), job(4, &["app"])]),
        repository("o/web", "web", vec![job(5, &["web"])]),
        repository("o/seeded", "seeded", vec![job(6, &["seeded"])]),
    ];
    let busy = Slot::Runner(RunnerSlot {
        repository: "o/web".to_owned(),
        busy: true,
        started_at: now(),
        memory: GIB,
        cpu: 1000,
    });
    let mut profiles = profiles(vec![Slot::Free, Slot::Free]);
    profiles.insert(
        "web".to_owned(),
        Profile {
            memory: GIB,
            cpus: 1000,
            preparing: None,
            upkeep_due: false,
            count: 1,
            slots: vec![busy],
        },
    );
    profiles.insert(
        "seeded".to_owned(),
        Profile {
            memory: GIB,
            cpus: 1000,
            preparing: Some(Preparing::Reseeding),
            upkeep_due: false,
            count: 1,
            slots: vec![Slot::Free],
        },
    );
    // Room for one 4G runner of app's, then 1G: job 3's start takes what job 4 would need.
    let plan = plan(&repositories, &profiles, budget(Some(5 * GIB), None), now());
    assert_eq!(starts(&plan.decisions), [(0, "o/a")]);
    assert_eq!(
        plan.waiting,
        BTreeMap::from([
            (1, Waiting::Paused),
            (
                2,
                Waiting::Backoff {
                    until: 1_000_060,
                    cause: HoldCause::NoJob
                }
            ),
            (4, Waiting::BehindStart { job: 3 }),
            (
                5,
                Waiting::NoSlot {
                    profile: "web".to_owned()
                }
            ),
            (
                6,
                Waiting::Reseeding {
                    profile: "seeded".to_owned()
                }
            ),
        ])
    );
}

#[test]
fn a_job_held_only_by_an_older_jobs_start_waits_behind_it_not_for_what_runs() {
    let two = |id| repository(&format!("o/{id}"), "app", vec![job(id, &["app"])]);
    // One free slot of two: job 1 takes it, job 2 is behind that start, not short of a busy slot.
    let busy = Slot::Runner(RunnerSlot {
        repository: "o/other".to_owned(),
        busy: true,
        started_at: now(),
        memory: GIB,
        cpu: 1000,
    });
    let plan = plan(
        &[two(1), two(2)],
        &profiles(vec![busy.clone(), Slot::Free]),
        budget(None, None),
        now(),
    );
    assert_eq!(starts(&plan.decisions), [(1, "o/1")]);
    assert_eq!(
        plan.waiting,
        BTreeMap::from([(2, Waiting::BehindStart { job: 1 })])
    );
    // With no slot free before the plan, the slots are busy.
    let plan = plan_of(&[two(1)], &profiles(vec![busy]), None, None);
    assert_eq!(
        plan.waiting,
        BTreeMap::from([(
            1,
            Waiting::NoSlot {
                profile: "app".to_owned()
            }
        )])
    );
    // Memory for one 4G runner: the second is behind the first; with 1G free, memory is short.
    let slots = || profiles(vec![Slot::Free, Slot::Free]);
    let behind = plan_of(&[two(1), two(2)], &slots(), Some(5 * GIB), None);
    assert_eq!(
        behind.waiting,
        BTreeMap::from([(2, Waiting::BehindStart { job: 1 })])
    );
    let short = plan_of(&[two(1)], &slots(), Some(GIB), None);
    assert_eq!(
        short.waiting,
        BTreeMap::from([(
            1,
            Waiting::Memory {
                needs: 4 * GIB,
                free: GIB
            }
        )])
    );
    // CPU likewise, while decisions stay as before: the second repository is held for CPU.
    let behind = plan_of(&[two(1), two(2)], &slots(), None, Some(3000));
    assert_eq!(
        behind.waiting,
        BTreeMap::from([(2, Waiting::BehindStart { job: 1 })])
    );
    assert!(
        behind.decisions.contains(&Decision::HeldForCpu {
            repository: "o/2".to_owned()
        }),
        "o/2 is held for CPU as before"
    );
}

fn plan_of(
    repositories: &[Repository],
    profiles: &BTreeMap<String, Profile>,
    memory: Option<u64>,
    cpu: Option<u64>,
) -> Plan {
    plan(repositories, profiles, budget(memory, cpu), now())
}

#[test]
fn a_job_an_idle_runner_will_take_waits_for_that_runner() {
    let repositories = [repository("o/a", "app", vec![job(1, &["app"])])];
    let plan = plan(
        &repositories,
        &profiles(vec![idle_runner("o/a", now())]),
        budget(None, None),
        now(),
    );
    assert_eq!(plan.decisions, []);
    assert_eq!(plan.waiting, BTreeMap::from([(1, Waiting::RunnerStarting)]));
}

#[test]
fn younger_jobs_wait_behind_one_starved_of_cpu() {
    let long_ago = now() - CPU_PRIORITY_AFTER;
    let repositories = [
        Repository {
            runner_cpu: Some(4000),
            cpu_waiting_since: Some(long_ago),
            ..repository("o/heavy", "app", vec![job(1, &["app"])])
        },
        Repository {
            runner_cpu: Some(500),
            ..repository("o/light", "app", vec![job(2, &["app"])])
        },
    ];
    let plan = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        budget(None, Some(1000)),
        now(),
    );
    assert_eq!(starts(&plan.decisions), []);
    assert_eq!(
        plan.waiting,
        BTreeMap::from([
            (
                1,
                Waiting::Cpu {
                    needs: 4000,
                    free: 1000
                }
            ),
            (2, Waiting::BehindCpu { job: 1 }),
        ])
    );
}

#[test]
fn the_idle_runner_of_a_paused_repository_stops_though_its_jobs_stay_queued() {
    let repositories = [Repository {
        paused: true,
        ..repository("o/a", "app", vec![job(1, &["app"])])
    }];
    let plan = plan(
        &repositories,
        &profiles(vec![idle_runner("o/a", now() - IDLE_TIMEOUT)]),
        budget(None, None),
        now(),
    );
    assert_eq!(
        plan.decisions,
        [Decision::Stop {
            slot: SlotRef {
                profile: "app".to_owned(),
                index: 0
            }
        }]
    );
    assert_eq!(plan.waiting, BTreeMap::from([(1, Waiting::Paused)]));
}

fn warm(name: &str, profile: &str, runners: usize) -> Repository {
    Repository {
        warm: runners,
        ..repository(name, profile, Vec::new())
    }
}

fn slot(index: usize) -> SlotRef {
    SlotRef {
        profile: "app".to_owned(),
        index,
    }
}

fn warm_starts(decisions: &[Decision]) -> Vec<(usize, &str)> {
    decisions
        .iter()
        .filter_map(|decision| match decision {
            Decision::Start {
                slot,
                repository,
                warm: true,
                ..
            } => Some((slot.index, repository.as_str())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_repository_wanting_a_warm_runner_starts_one_with_no_job_queued() {
    let plan = plan(
        &[warm("o/a", "app", 1)],
        &profiles(vec![Slot::Free, Slot::Free]),
        budget(None, None),
        now(),
    );
    assert_eq!(warm_starts(&plan.decisions), [(0, "o/a")]);
    assert_eq!(
        plan.decisions,
        [Decision::Start {
            slot: slot(0),
            repository: "o/a".to_owned(),
            memory: 4 * GIB,
            cpu: 2000,
            warm: true,
            replacing: Vec::new(),
        }]
    );
}

#[test]
fn a_warm_runner_is_kept_past_the_idle_timeout_and_counts_as_warm() {
    let plan = plan(
        &[warm("o/a", "app", 1)],
        &profiles(vec![
            idle_runner("o/a", now() - 10 * IDLE_TIMEOUT),
            Slot::Free,
        ]),
        budget(None, None),
        now(),
    );
    assert_eq!(plan.decisions, []);
    assert_eq!(plan.warm, [slot(0)].into());
}

#[test]
fn an_idle_runner_beyond_the_warm_ones_stops_after_the_idle_timeout() {
    let long_ago = now() - IDLE_TIMEOUT;
    let plan = plan(
        &[warm("o/a", "app", 1)],
        &profiles(vec![
            idle_runner("o/a", long_ago),
            idle_runner("o/a", long_ago),
        ]),
        budget(None, None),
        now(),
    );
    assert_eq!(plan.decisions, [Decision::Stop { slot: slot(0) }]);
    assert_eq!(plan.warm, [slot(1)].into());
}

#[test]
fn a_queued_job_counts_on_the_idle_runner_and_a_new_warm_runner_starts_beside_it() {
    let repositories = [Repository {
        warm: 1,
        ..repository("o/a", "app", vec![job(1, &["app"])])
    }];
    let plan = plan(
        &repositories,
        &profiles(vec![idle_runner("o/a", now()), Slot::Free]),
        budget(None, None),
        now(),
    );
    assert_eq!(starts(&plan.decisions), [(1, "o/a")]);
    assert_eq!(warm_starts(&plan.decisions), [(1, "o/a")]);
    assert_eq!(plan.waiting, BTreeMap::from([(1, Waiting::RunnerStarting)]));
}

#[test]
fn a_queued_job_takes_the_slot_of_a_warm_runner_of_another_repository_in_the_same_plan() {
    let repositories = [
        warm("o/a", "app", 1),
        repository("o/b", "app", vec![job(1, &["app"])]),
    ];
    let plan = plan(
        &repositories,
        &profiles(vec![idle_runner("o/a", now())]),
        budget(None, None),
        now(),
    );
    assert_eq!(
        plan.decisions,
        [
            Decision::Stop { slot: slot(0) },
            Decision::Start {
                slot: slot(0),
                repository: "o/b".to_owned(),
                memory: 4 * GIB,
                cpu: 2000,
                warm: false,
                replacing: vec![slot(0)],
            }
        ]
    );
    assert_eq!(plan.waiting, BTreeMap::new());
}

#[test]
fn a_queued_job_takes_the_memory_and_cpu_of_warm_runners_of_another_profile() {
    let mut profiles = profiles(vec![Slot::Free]);
    profiles.insert(
        "web".to_owned(),
        Profile {
            memory: 4 * GIB,
            cpus: 2000,
            preparing: None,
            upkeep_due: false,
            count: 2,
            slots: vec![idle_runner("o/w", now()), idle_runner("o/w", now())],
        },
    );
    let repositories = [
        Repository {
            warm: 2,
            ..repository("o/w", "web", Vec::new())
        },
        repository("o/a", "app", vec![job(1, &["app"])]),
    ];
    // 1G and 1 CPU left: the job needs 4G and 2 CPUs, which one warm runner gives back.
    let plan = plan(
        &repositories,
        &profiles,
        budget(Some(GIB), Some(1000)),
        now(),
    );
    let web = |index| SlotRef {
        profile: "web".to_owned(),
        index,
    };
    assert_eq!(
        plan.decisions,
        [
            Decision::Stop { slot: web(0) },
            Decision::Start {
                slot: slot(0),
                repository: "o/a".to_owned(),
                memory: 4 * GIB,
                cpu: 2000,
                warm: false,
                replacing: vec![web(0)],
            }
        ]
    );
    assert_eq!(plan.warm, [web(1)].into());
}

#[test]
fn warm_runners_that_cannot_make_room_together_stay() {
    let repositories = [
        warm("o/a", "app", 1),
        Repository {
            runner_memory: Some(16 * GIB),
            ..repository("o/b", "app", vec![job(1, &["app"])])
        },
    ];
    let plan = plan(
        &repositories,
        &profiles(vec![idle_runner("o/a", now())]),
        budget(Some(GIB), None),
        now(),
    );
    assert_eq!(plan.decisions, []);
    assert_eq!(plan.warm, [slot(0)].into());
    assert_eq!(
        plan.waiting,
        BTreeMap::from([(
            1,
            Waiting::NoSlot {
                profile: "app".to_owned()
            }
        )])
    );
}

#[test]
fn a_warm_runner_never_makes_room_for_another_warm_runner() {
    let plan = plan(
        &[warm("o/a", "app", 1), warm("o/b", "app", 1)],
        &profiles(vec![idle_runner("o/a", now())]),
        budget(None, None),
        now(),
    );
    assert_eq!(plan.decisions, []);
}

#[test]
fn while_the_slice_is_held_warm_runners_make_no_room_and_none_starts() {
    let repositories = [
        warm("o/a", "app", 1),
        repository("o/b", "app", vec![job(1, &["app"])]),
    ];
    let held = Budget {
        memory: Some(0),
        cpu: None,
        host: None,
        held: true,
    };
    let plan = plan(
        &repositories,
        &profiles(vec![idle_runner("o/a", now()), Slot::Free]),
        held,
        now(),
    );
    assert_eq!(plan.decisions, []);
}

#[test]
fn no_warm_runner_starts_while_a_job_waits_for_memory() {
    let repositories = [
        Repository {
            runner_memory: Some(8 * GIB),
            ..repository("o/b", "app", vec![job(1, &["app"])])
        },
        warm("o/a", "app", 1),
    ];
    let plan = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        budget(Some(5 * GIB), None),
        now(),
    );
    assert_eq!(plan.decisions, []);
}

#[test]
fn warm_runners_start_only_within_the_budget_one_per_repository_first() {
    let repositories = [warm("o/a", "app", 3), warm("o/b", "app", 3)];
    let plan = plan(
        &repositories,
        &profiles(vec![Slot::Free; 4]),
        budget(Some(12 * GIB), Some(100_000)),
        now(),
    );
    assert_eq!(
        warm_starts(&plan.decisions),
        [(0, "o/a"), (1, "o/b"), (2, "o/a")]
    );
}

#[test]
fn a_profile_due_a_reseed_stops_its_warm_runners_at_once_and_starts_none() {
    let mut profiles = profiles(vec![idle_runner("o/a", now()), Slot::Free]);
    profiles.get_mut("app").unwrap().upkeep_due = true;
    let plan = plan(
        &[warm("o/a", "app", 1)],
        &profiles,
        budget(None, None),
        now(),
    );
    assert_eq!(plan.decisions, [Decision::Stop { slot: slot(0) }]);
    assert!(plan.warm.is_empty(), "no runner is kept warm");
}

#[test]
fn a_paused_repository_keeps_no_warm_runner() {
    let plan = plan(
        &[Repository {
            paused: true,
            ..warm("o/a", "app", 1)
        }],
        &profiles(vec![idle_runner("o/a", now() - IDLE_TIMEOUT), Slot::Free]),
        budget(None, None),
        now(),
    );
    assert_eq!(plan.decisions, [Decision::Stop { slot: slot(0) }]);
}

#[test]
fn a_warm_runner_in_a_slot_a_lowered_count_closed_stops_and_its_slot_serves_no_job() {
    // `slots` went from 2 to 1 while o/a's warm runner waited in slot 2; slot 1 is closed while it runs there.
    let mut profiles = profiles(vec![Slot::Closed, idle_runner("o/a", now())]);
    profiles.get_mut("app").unwrap().count = 1;
    let stopped = plan(
        &[warm("o/a", "app", 1)],
        &profiles,
        budget(None, None),
        now(),
    );
    assert_eq!(
        stopped.decisions,
        [Decision::Stop { slot: slot(1) }],
        "it stops, and slot 1 takes the next warm runner"
    );

    // Another repository's job finds no room in it either.
    let other = plan(
        &[
            warm("o/a", "app", 1),
            repository("o/b", "app", vec![job(1, &["self-hosted", "linux", "app"])]),
        ],
        &profiles,
        budget(None, None),
        now(),
    );
    assert!(
        starts(&other.decisions).is_empty(),
        "no runner starts in a closed slot: {:?}",
        other.decisions
    );
}
