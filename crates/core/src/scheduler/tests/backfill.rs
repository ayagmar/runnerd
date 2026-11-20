//! What starts behind a barrier (ADR 0097), as `plan` decides it: the rule's four parts, each failing on its own; over
//! generated runs and over every small world, its two bounds and what run times that read long cost.

use super::*;
use crate::scheduler::{Drained, PRIORITY_AFTER, Retired, SHORT_RUN};

const MINUTE: Duration = Duration::from_secs(60);
/// When the owner's wait began.
const WAITED: Duration = Duration::from_mins(15);
/// Started before the owner's barrier matured.
const BEFORE: Duration = Duration::from_mins(10);

fn minutes(minutes: u32) -> Duration {
    MINUTE * minutes
}

fn starved(job: u64) -> Waits {
    Waits::default().with_memory(Some(Wait {
        job,
        since: now() - WAITED,
    }))
}

/// A pool of `name` in `profile` with runners of `memory`, usually done in `usual`.
fn pool(
    name: &str,
    profile: &str,
    memory: u64,
    usual: Option<Duration>,
    queued: &[u64],
) -> Repository {
    Repository {
        runner_memory: Some(memory),
        runner_cpu: Some(0),
        usual,
        ..repository(
            name,
            profile,
            queued.iter().map(|id| job(*id, &[profile])).collect(),
        )
    }
}

/// o/big's job 1, 8G, past its limit.
fn owner() -> Repository {
    Repository {
        waits: starved(1),
        ..pool("o/big", "big", 8 * GIB, None, &[1])
    }
}

/// o/small's job 2, 1G, younger than the owner.
fn younger(usual: Option<Duration>) -> Repository {
    pool("o/small", "small", GIB, usual, &[2])
}

/// A runner that started before the barrier matured, takes `memory`, and usually ends `ends_in` from now.
fn running(memory: u64, ends_in: Duration) -> Slot {
    Slot::Runner(RunnerSlot {
        repository: "o/other".to_owned(),
        busy: true,
        started_at: now() - BEFORE,
        memory,
        cpu: 0,
        usual: Some(BEFORE + ends_in),
        caps: Caps { memory, cpu: 0 },
        behind: None,
    })
}

fn profile(memory: u64, cpus: u64, slots: Vec<Slot>) -> Profile {
    Profile {
        memory,
        cpus,
        preparing: None,
        upkeep_due: false,
        count: slots.len(),
        ran: Vec::new(),
        slots,
    }
}

/// `big` 2 × 8G, `small` 4 × 1G, and `other` holding `runners`, at 8G each at most.
fn slots(runners: Vec<Slot>) -> BTreeMap<String, Profile> {
    BTreeMap::from([
        (
            "big".to_owned(),
            profile(8 * GIB, 1000, vec![Slot::Free; 2]),
        ),
        ("small".to_owned(), profile(GIB, 1000, vec![Slot::Free; 4])),
        ("other".to_owned(), profile(8 * GIB, 1000, runners)),
    ])
}

/// A 16G slice with `used` of it taken.
fn slice(used: u64) -> Budget {
    Budget {
        memory: Some(super::super::left(16 * GIB, used)),
        drained: Drained {
            memory: Some(super::super::left(16 * GIB, 0)),
            ..Drained::default()
        },
        ..Budget::default()
    }
}

/// 12G of 16G in use, 4G of it back in 20 minutes: the owner's room comes then.
fn room_in_twenty_minutes() -> Vec<Slot> {
    vec![running(4 * GIB, minutes(20)), running(8 * GIB, minutes(40))]
}

fn plan_behind(younger_pool: Repository, runners: Vec<Slot>) -> Plan {
    let used = runners
        .iter()
        .map(|slot| match slot {
            Slot::Runner(runner) => runner.memory,
            Slot::Free | Slot::Closed => 0,
        })
        .sum();
    plan(
        &[owner(), younger_pool],
        &slots(runners),
        slice(used),
        now(),
    )
}

fn started(plan: &Plan) -> Vec<&str> {
    starts(&plan.decisions)
        .into_iter()
        .map(|(_, repository)| repository)
        .collect()
}

#[track_caller]
fn assert_passes(plan: &Plan) {
    assert_eq!(started(plan), ["o/small"], "{plan:?}");
    assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(1));
    assert!(
        !plan.waiting.contains_key(&2),
        "a job that passes has no reason to wait: {plan:?}"
    );
}

#[track_caller]
fn assert_held(plan: &Plan) {
    assert_eq!(started(plan), [] as [&str; 0], "{plan:?}");
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

#[test]
fn a_younger_job_that_usually_ends_before_the_owners_room_gathers_starts_behind_it() {
    assert_passes(&plan_behind(
        younger(Some(minutes(10))),
        room_in_twenty_minutes(),
    ));
}

/// The limit is the owner's start under a strict barrier: past it, the younger job's memory is still taken.
#[test]
fn a_younger_job_may_end_as_the_room_gathers_and_not_a_moment_later() {
    let at_the_end = younger(Some(minutes(20)));
    assert_passes(&plan_behind(at_the_end, room_in_twenty_minutes()));
    let past_it = younger(Some(minutes(20) + Duration::from_secs(1)));
    assert_held(&plan_behind(past_it, room_in_twenty_minutes()));
}

/// The log line a sandbox run counts passes by reads this, so a start before the barrier must not carry it.
#[test]
fn a_start_says_which_barrier_it_passed() {
    let older = pool("o/older", "small", GIB, Some(minutes(5)), &[0]);
    let plan = plan(
        &[older, owner(), younger(Some(minutes(1)))],
        &slots(room_in_twenty_minutes()),
        slice(13 * GIB),
        now(),
    );
    let behind: Vec<Option<u64>> = plan
        .decisions
        .iter()
        .filter_map(|decision| match decision {
            Decision::Start { behind, .. } => Some(*behind),
            Decision::Stop { .. } => None,
        })
        .collect();
    assert_eq!(behind, [None, Some(1)]);
}

#[test]
fn a_pool_with_no_estimate_stays_behind_the_barrier() {
    assert_held(&plan_behind(younger(None), room_in_twenty_minutes()));
}

/// An overdue runner may end any moment, so no time is safe to fill: only a short job passes (ADR 0151).
#[test]
fn only_a_short_job_passes_while_a_runner_that_gives_the_room_is_past_its_usual_end() {
    let overdue = Slot::Runner(RunnerSlot {
        repository: "o/other".to_owned(),
        busy: true,
        started_at: now() - BEFORE,
        memory: 4 * GIB,
        cpu: 0,
        usual: Some(BEFORE.saturating_sub(MINUTE)),
        caps: Caps {
            memory: 4 * GIB,
            cpu: 0,
        },
        behind: None,
    });
    let runners = || vec![overdue.clone(), running(8 * GIB, minutes(40))];
    let past_short = SHORT_RUN + Duration::from_secs(1);
    assert_held(&plan_behind(younger(Some(past_short)), runners()));
    assert_passes(&plan_behind(younger(Some(SHORT_RUN)), runners()));
}

#[test]
fn a_runner_with_no_estimate_or_no_job_counts_as_ending_now() {
    let unknown = |busy, usual| {
        Slot::Runner(RunnerSlot {
            repository: "o/other".to_owned(),
            busy,
            started_at: now() - BEFORE,
            memory: 4 * GIB,
            cpu: 0,
            usual,
            caps: Caps {
                memory: 4 * GIB,
                cpu: 0,
            },
            behind: None,
        })
    };
    for runner in [unknown(true, None), unknown(false, Some(minutes(60)))] {
        let runners = vec![runner, running(8 * GIB, minutes(40))];
        assert_held(&plan_behind(younger(Some(minutes(3))), runners));
    }
}

/// Room held by something no runner's end gives back: the 4G that ends leaves the owner short, so no time is known, and
/// only a short job passes (ADR 0151).
#[test]
fn only_a_short_job_passes_while_no_runners_end_brings_the_room() {
    let plan_with = |left, usual| {
        plan(
            &[owner(), younger(Some(usual))],
            &slots(vec![running(4 * GIB, minutes(20))]),
            Budget {
                memory: Some(left),
                ..slice(0)
            },
            now(),
        )
    };
    assert_passes(&plan_with(gib(4), minutes(3)));
    assert_held(&plan_with(gib(3), minutes(3)));
    assert_passes(&plan_with(gib(3), minutes(1)));
}

fn gib(amount: i64) -> i64 {
    amount * i64::try_from(GIB).unwrap()
}

/// A runner of `o/other` past its usual end, taking `memory`: the room it gives may come any moment, so no time is known.
fn overdue(memory: u64) -> Slot {
    Slot::Runner(RunnerSlot {
        repository: "o/other".to_owned(),
        busy: true,
        started_at: now() - BEFORE,
        memory,
        cpu: 0,
        usual: Some(Duration::from_mins(9)),
        caps: Caps { memory, cpu: 0 },
        behind: None,
    })
}

/// A short job of the owner's profile would take one of its slots: it passes by its run time alone, as before.
#[test]
fn a_short_job_of_the_owners_profile_passes_by_its_run_time_alone() {
    let sibling = pool("o/sibling", "big", GIB, Some(minutes(1)), &[2]);
    let plan = plan(
        &[owner(), sibling],
        &slots(vec![overdue(4 * GIB), running(8 * GIB, minutes(40))]),
        Budget {
            drained: Drained {
                memory: Some(gib(32)),
                ..Drained::default()
            },
            ..slice(12 * GIB)
        },
        now(),
    );
    assert_eq!(started(&plan), [] as [&str; 0], "{plan:?}");
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

/// Once only what passed a barrier keeps the owner out, its room has come: a short job that took it next would keep the
/// owner waiting a run longer each time, for as long as short jobs keep coming.
#[test]
fn no_short_job_passes_once_only_what_passed_keeps_the_owner_out() {
    let since = now() - WAITED + PRIORITY_AFTER;
    let passed = Slot::Runner(RunnerSlot {
        repository: "o/small".to_owned(),
        busy: true,
        started_at: since,
        memory: GIB,
        cpu: 0,
        usual: Some(minutes(1)),
        caps: Caps {
            memory: GIB,
            cpu: 1000,
        },
        behind: Some(1),
    });
    // 4G that may end any moment and `other` beside it: with the passed 1G back, 7G or 8G are left for the 8G owner.
    let plan_with = |other| {
        let mut profiles = slots(vec![overdue(4 * GIB), running(other, minutes(40))]);
        profiles.get_mut("small").unwrap().slots[0] = passed.clone();
        plan(
            &[owner(), younger(Some(minutes(1)))],
            &profiles,
            slice(5 * GIB + other),
            now(),
        )
    };
    assert_passes(&plan_with(5 * GIB));
    assert_held(&plan_with(4 * GIB));
}

/// A short job keeps to the caps of what passed: whatever it does, the owner still fits once the rest has ended.
#[test]
fn a_short_job_leaves_the_owner_its_room_at_its_caps() {
    let plan_with = |drain| {
        plan(
            &[owner(), younger(Some(minutes(1)))],
            &slots(vec![overdue(4 * GIB), running(8 * GIB, minutes(40))]),
            Budget {
                drained: Drained {
                    memory: Some(drain),
                    ..Drained::default()
                },
                ..slice(12 * GIB)
            },
            now(),
        )
    };
    assert_passes(&plan_with(gib(9)));
    assert_held(&plan_with(gib(9) - 1));
}

/// A job the barrier lets pass is held only by what it lacks, so it says so. One the host holds is then a waiter the
/// host's band reads (ADR 0102): said to wait behind the barrier, it would keep the larger margin up for nothing.
#[test]
fn a_job_the_barrier_lets_pass_says_what_it_lacks() {
    let plan_with = |usual| {
        plan(
            &[owner(), younger(Some(usual))],
            &slots(vec![overdue(4 * GIB), running(8 * GIB, minutes(40))]),
            Budget {
                host: Some(gib(0)),
                ..slice(12 * GIB)
            },
            now(),
        )
    };
    let passes = plan_with(minutes(1));
    assert_eq!(started(&passes), [] as [&str; 0]);
    assert_eq!(
        passes.waiting[&2],
        Waiting::HostMemory {
            needs: GIB,
            free: 0
        }
    );
    // Past the owner's room in 40 minutes, and too long to pass as a short job.
    assert_eq!(
        plan_with(minutes(50)).waiting[&2],
        Waiting::BehindMemory { job: 1 }
    );
}

/// A short job that outlasts an older job's wait delays it, and that one's end, and the next older job's start after it:
/// short passes could chain to the owner however its own room stands. With exact run times in 10G, a 2G runner until
/// 300 s and a 5G one until 1 s, older jobs A (6G) and B (7G) of 30 s and the 7G owner start at 1, 31 and 61 s under a
/// strict barrier. A 1G two-minute job that passed at 0 would hold A to 120 s, and another that passed as A started, B
/// to 240 s, as only 6G is free until it ends: the owner would start at 270 s.
#[test]
fn a_short_job_ends_before_an_older_job_could_start() {
    let plan_at = |runners: Vec<Slot>, older: &[(&str, u64)]| {
        let used = runners
            .iter()
            .map(|slot| match slot {
                Slot::Runner(runner) => runner.memory,
                Slot::Free | Slot::Closed => 0,
            })
            .sum();
        let mut profiles = slots(runners);
        let mut repositories = Vec::new();
        for (id, (name, needs)) in (3..).zip(older) {
            profiles.insert((*name).to_owned(), profile(*needs, 1000, vec![Slot::Free]));
            repositories.push(pool(&format!("o/{name}"), name, *needs, None, &[id]));
        }
        repositories.push(Repository {
            waits: starved(5),
            ..pool("o/big", "big", 7 * GIB, None, &[5])
        });
        repositories.push(pool("o/small", "small", GIB, Some(SHORT_RUN), &[6]));
        let plan = plan(
            &repositories,
            &profiles,
            Budget {
                memory: Some(super::super::left(10 * GIB, used)),
                drained: Drained {
                    memory: Some(gib(10)),
                    ..Drained::default()
                },
                ..Budget::default()
            },
            now(),
        );
        assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(5));
        started(&plan).contains(&"o/small")
    };
    let secs = Duration::from_secs;
    let at_0 = || vec![running(2 * GIB, secs(300)), running(5 * GIB, secs(1))];
    assert!(
        !plan_at(at_0(), &[("a", 6 * GIB), ("b", 7 * GIB)]),
        "at 0 A could start in 1 s"
    );
    // At 120 s, with A running: B could start in 30 s, or in 10 minutes had A that long to run.
    let at_120 = |a_ends_in| vec![running(2 * GIB, secs(180)), running(6 * GIB, a_ends_in)];
    assert!(
        !plan_at(at_120(secs(30)), &[("b", 7 * GIB)]),
        "B could start in 30 s"
    );
    assert!(
        plan_at(at_120(minutes(10)), &[("b", 7 * GIB)]),
        "B could start in 10 minutes"
    );
}

/// A reload lowers a profile's caps for the runners it starts next, not for those that run: a runner that passed
/// counts at the caps it started with, as its container keeps them. One whose profile the reload removed still runs
/// and counts too. 12G; the 8G owner waits for a 9G runner; a drain leaves 4G past the owner, which a passed runner of
/// 4G caps took, so the 1G job of 3G caps waits. Read at its profile's 1G now, 3G would seem left for it.
#[test]
fn a_runner_that_passed_counts_at_the_caps_it_started_with() {
    let since = now() - WAITED + PRIORITY_AFTER;
    let passed = RunnerSlot {
        repository: "o/p".to_owned(),
        busy: true,
        started_at: since,
        memory: GIB,
        cpu: 0,
        usual: Some(WAITED),
        caps: Caps {
            memory: 4 * GIB,
            cpu: 0,
        },
        behind: Some(1),
    };
    let plan_with = |p_slots: Vec<Slot>, retired: Vec<Retired>| {
        let mut profiles = slots(vec![running(9 * GIB, minutes(40))]);
        profiles.insert("p".to_owned(), profile(GIB, 0, p_slots));
        profiles.insert("q".to_owned(), profile(3 * GIB, 0, vec![Slot::Free]));
        plan(
            &[owner(), pool("o/q", "q", GIB, Some(minutes(1)), &[2])],
            &profiles,
            Budget {
                memory: Some(super::super::left(12 * GIB, 10 * GIB)),
                drained: Drained {
                    memory: Some(gib(12)),
                    ..Drained::default()
                },
                retired,
                ..Budget::default()
            },
            now(),
        )
    };
    let reloaded = plan_with(vec![Slot::Runner(passed.clone())], Vec::new());
    assert_eq!(started(&reloaded), [] as [&str; 0], "{reloaded:?}");
    assert_eq!(reloaded.waiting[&2], Waiting::BehindMemory { job: 1 });
    let removed = plan_with(
        vec![Slot::Free],
        vec![Retired {
            profile: "gone".to_owned(),
            runner: passed.clone(),
        }],
    );
    assert_eq!(started(&removed), [] as [&str; 0], "{removed:?}");
    // Started at 1G caps, it leaves the 3G.
    let small = RunnerSlot {
        caps: Caps {
            memory: GIB,
            cpu: 0,
        },
        ..passed
    };
    assert_eq!(
        started(&plan_with(vec![Slot::Runner(small)], Vec::new())),
        ["o/q"]
    );
}

/// Under the larger host margin the room comes later than under the plain one, so a backfill read by the margin in force
/// would let a job pass under the larger one and not under the plain one. Said to wait for the host under the larger
/// margin, it would let the band go; said to wait behind the barrier under the plain one, it would bring the band back,
/// plan after plan. Host room of 4G, or 6G by the plain margin; a 10G runner whose end is not known and a passed 2G one:
/// with the latter back, the 8G owner fits by the plain margin, so the 5G short job waits behind the barrier by either.
#[test]
fn a_job_passes_by_the_plain_margin_whichever_stands() {
    let since = now() - WAITED + PRIORITY_AFTER;
    let plan_by = |host: i64, host_larger_by: u64, needs: u64, cpu: Option<i64>| {
        let passed = Slot::Runner(RunnerSlot {
            repository: "o/small".to_owned(),
            busy: true,
            started_at: since,
            memory: 2 * GIB,
            cpu: 0,
            usual: Some(WAITED),
            caps: Caps {
                memory: 2 * GIB,
                cpu: 0,
            },
            behind: Some(1),
        });
        let mut profiles = slots(vec![Slot::Runner(RunnerSlot {
            usual: None,
            ..match overdue(10 * GIB) {
                Slot::Runner(runner) => runner,
                Slot::Free | Slot::Closed => unreachable!(),
            }
        })]);
        profiles.get_mut("small").unwrap().slots[0] = passed;
        profiles.insert("mid".to_owned(), profile(needs, 1000, vec![Slot::Free]));
        let mid = Repository {
            runner_cpu: Some(1000),
            ..pool("o/mid", "mid", needs, Some(minutes(1)), &[2])
        };
        plan(
            &[owner(), mid],
            &profiles,
            Budget {
                host: Some(host),
                host_larger_by,
                cpu,
                drained: Drained {
                    host: Some(gib(16)),
                    host_plain: Some(gib(18)),
                    cpu: Some(16_000),
                    ..Drained::default()
                },
                ..Budget::default()
            },
            now(),
        )
    };
    let larger = plan_by(gib(4), 2 * GIB, 5 * GIB, None);
    let plain = plan_by(gib(6), 0, 5 * GIB, None);
    for plan in [&larger, &plain] {
        assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(1));
        assert_eq!(
            plan.waiting[&2],
            Waiting::BehindMemory { job: 1 },
            "{plan:?}"
        );
    }
    assert_eq!(larger.host_needs().collect::<Vec<_>>(), [8 * GIB]);
}

/// A job that waits for the host's memory but lacks CPU too would not start by the plain margin either: read as one that
/// would, it lets the band go, says it waits for CPU under the plain margin, and the band comes back (ADR 0102).
#[test]
fn a_job_short_of_cpu_too_is_no_waiter_the_plain_margin_starts() {
    let plan_with = |cpu: i64| {
        let mut profiles = slots(Vec::new());
        profiles.insert("mid".to_owned(), profile(3 * GIB, 1000, vec![Slot::Free]));
        plan(
            &[Repository {
                runner_cpu: Some(1000),
                ..pool("o/mid", "mid", 3 * GIB, None, &[2])
            }],
            &profiles,
            Budget {
                host: Some(gib(2)),
                host_larger_by: 2 * GIB,
                cpu: Some(cpu),
                ..Budget::default()
            },
            now(),
        )
    };
    let fits = plan_with(1000);
    let short = plan_with(500);
    for plan in [&fits, &short] {
        assert!(
            matches!(plan.waiting[&2], Waiting::HostMemory { .. }),
            "{plan:?}"
        );
    }
    assert_eq!(fits.host_needs().collect::<Vec<_>>(), [3 * GIB]);
    assert_eq!(short.host_needs().collect::<Vec<_>>(), [u64::MAX]);
}

/// What passed since the barrier matured may hang and grow to its caps: at its caps, it must leave the owner its room.
/// What started for an older job, before the barrier matured, or as a warm runner did not pass, and the owner waits it
/// out under a strict barrier too.
#[test]
fn the_caps_of_what_passed_since_the_barrier_matured_leave_the_owner_its_room() {
    let since = now() - WAITED + PRIORITY_AFTER;
    // A 1G runner in `profile`, since `started_at`: `other` caps it at 8G, `small` at 1G. Beside it, 4G back in 20
    // minutes brings the owner's 8G; a drain gives 16G, so what passed may have caps of 8G less the 1G younger job.
    let plan_with = |profile: &str, started_at, behind| {
        let mut profiles = slots(vec![
            running(4 * GIB, minutes(20)),
            running(6 * GIB + GIB / 4, minutes(40)),
        ]);
        let into = profiles.get_mut(profile).unwrap();
        let caps = Caps {
            memory: into.memory,
            cpu: into.cpus,
        };
        let slots = &mut into.slots;
        let free = slots.iter().position(|slot| *slot == Slot::Free);
        let runner = Slot::Runner(RunnerSlot {
            repository: "o/other".to_owned(),
            busy: true,
            started_at,
            memory: GIB,
            cpu: 0,
            usual: Some(minutes(600)),
            caps,
            behind,
        });
        match free {
            Some(index) => slots[index] = runner,
            None => slots.push(runner),
        }
        plan(
            &[owner(), younger(Some(minutes(1)))],
            &profiles,
            slice(11 * GIB + GIB / 4),
            now(),
        )
    };
    assert_passes(&plan_with("small", since, Some(1)));
    // At its cap, not at the 1G it takes.
    assert_held(&plan_with("other", since, Some(1)));
    // Before the barrier matured, it is what a strict barrier waits out too.
    let before = since - Duration::from_secs(1);
    assert_passes(&plan_with("other", before, Some(1)));
    // Started since for an older job: not what passed.
    assert_passes(&plan_with("other", since, None));
}

/// An idle runner counts as ending now for when the room gathers, so its room only binds where the owner needs more
/// than it and the runner that ends in 20 minutes: here 10G of a 16G drain, with 8G running.
#[test]
fn a_passed_runner_github_gave_no_job_holds_its_room_and_a_warm_one_does_not() {
    let since = now() - WAITED + PRIORITY_AFTER;
    let idle = |behind| {
        Slot::Runner(RunnerSlot {
            repository: "o/other".to_owned(),
            busy: false,
            started_at: since,
            memory: 6 * GIB,
            cpu: 0,
            usual: None,
            caps: Caps {
                memory: 6 * GIB,
                cpu: 0,
            },
            behind,
        })
    };
    let owner = Repository {
        waits: starved(1),
        ..pool("o/big", "big", 10 * GIB, None, &[1])
    };
    let plan_with = |idle| {
        plan(
            &[owner.clone(), younger(Some(minutes(1)))],
            &slots(vec![running(8 * GIB, minutes(20)), idle]),
            slice(14 * GIB),
            now(),
        )
    };
    // A twin, or a runner whose job went elsewhere: 16G less the owner's 10G and its 8G cap leaves nothing.
    assert_held(&plan_with(idle(Some(1))));
    // A warm runner never starts behind a barrier.
    assert_passes(&plan_with(idle(None)));
}

#[test]
fn the_caps_of_a_plans_own_passes_count_too() {
    let younger = pool("o/small", "small", GIB, Some(minutes(1)), &[2, 3, 4]);
    let budget = Budget {
        drained: Drained {
            memory: Some(gib(10)),
            ..Drained::default()
        },
        ..slice(12 * GIB)
    };
    let plan = plan(
        &[owner(), younger],
        &slots(room_in_twenty_minutes()),
        budget,
        now(),
    );
    assert_eq!(started(&plan), ["o/small", "o/small"], "two 1G caps fit 2G");
    assert_eq!(plan.waiting[&4], Waiting::BehindMemory { job: 1 });
}

#[test]
fn nothing_passes_under_an_interlock() {
    let budget = Budget {
        memory: Some(0),
        held: true,
        ..slice(12 * GIB)
    };
    let plan = plan(
        &[owner(), younger(Some(minutes(1)))],
        &slots(room_in_twenty_minutes()),
        budget,
        now(),
    );
    assert_eq!(started(&plan), [] as [&str; 0]);
    assert_eq!(plan.barrier, None, "an interlock suspends the barrier");
}

/// The host's room can come later than the slice's: a backfill that read only the slice would take what the host owes.
#[test]
fn the_room_gathers_when_the_host_has_it_too() {
    let budget = |host| Budget {
        host: Some(host),
        ..slice(12 * GIB)
    };
    let plan_with = |host, usual| {
        plan(
            &[owner(), younger(Some(usual))],
            &slots(room_in_twenty_minutes()),
            budget(host),
            now(),
        )
    };
    // With 4G, the first end gives the host 8G; with 1G, only the second does.
    assert_passes(&plan_with(gib(4), minutes(20)));
    assert_held(&plan_with(gib(4), minutes(21)));
    assert_passes(&plan_with(gib(1), minutes(40)));
}

#[test]
fn the_room_gathers_when_the_cpu_has_it_too() {
    let busy = |cpu, ends_in| {
        let Slot::Runner(runner) = running(GIB, ends_in) else {
            unreachable!()
        };
        Slot::Runner(RunnerSlot { cpu, ..runner })
    };
    let owner = Repository {
        runner_cpu: Some(4000),
        ..pool("o/big", "big", GIB, None, &[1])
    };
    let starved_of_cpu = Waits::default().with_cpu(Some(Wait {
        job: 1,
        since: now() - WAITED,
    }));
    let budget = Budget {
        cpu: Some(1000),
        drained: Drained {
            cpu: Some(8000),
            memory: Some(gib(16)),
            ..Drained::default()
        },
        ..slice(2 * GIB)
    };
    let plan_with = |usual| {
        plan(
            &[
                Repository {
                    waits: starved_of_cpu,
                    ..owner.clone()
                },
                younger(Some(usual)),
            ],
            &slots(vec![busy(2000, minutes(10)), busy(5000, minutes(30))]),
            budget.clone(),
            now(),
        )
    };
    assert_eq!(started(&plan_with(minutes(30))), ["o/small"]);
    assert_eq!(
        plan_with(minutes(31)).waiting[&2],
        Waiting::BehindCpu { job: 1 }
    );
}

/// A reload that changes what the owner needs reaches the next plan: nothing of an earlier one is kept.
#[test]
fn what_the_owner_needs_is_read_again_by_every_plan() {
    let plan_for = |memory| {
        let needing = Repository {
            runner_memory: Some(memory),
            ..owner()
        };
        plan(
            &[needing, younger(Some(minutes(30)))],
            &slots(room_in_twenty_minutes()),
            slice(12 * GIB),
            now(),
        )
    };
    assert_held(&plan_for(8 * GIB));
    assert_eq!(
        started(&plan_for(9 * GIB)),
        ["o/small"],
        "its room is later"
    );
}

/// A runner started in this plan has no job yet. GitHub may give it a shorter one than its pool's longest, so reading
/// that as its end would put the owner's room too late: found by the generated runs below.
#[test]
fn a_runner_started_in_the_same_plan_counts_as_ending_now() {
    let older = pool("o/older", "small", 4 * GIB, Some(minutes(30)), &[0]);
    let plan = plan(
        &[older, owner(), younger(Some(minutes(3)))],
        &slots(vec![running(5 * GIB, minutes(40))]),
        slice(5 * GIB),
        now(),
    );
    assert_eq!(started(&plan), ["o/older"]);
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

/// A job older than the owner goes first once it fits. Room it loses to a younger job delays it, and what it then runs
/// late delays the owner, whose own room came on time: found by the generated runs below.
#[test]
fn a_younger_job_ends_before_a_job_older_than_the_owner_could_start() {
    let older = |id| pool("o/older", "older", 6 * GIB, None, &[id]);
    let plan_with = |usual| {
        let mut profiles = slots(vec![
            running(3 * GIB, minutes(5)),
            running(10 * GIB, minutes(20)),
        ]);
        profiles.insert("older".to_owned(), profile(6 * GIB, 1000, vec![Slot::Free]));
        // 3G left: the older job's 6G gather at minute 5, the owner's 8G at minute 20.
        let owner = Repository {
            waits: starved(5),
            ..pool("o/big", "big", 8 * GIB, None, &[5])
        };
        plan(
            &[
                older(1),
                owner,
                pool("o/small", "small", GIB, Some(usual), &[6]),
            ],
            &profiles,
            slice(13 * GIB),
            now(),
        )
    };
    assert_eq!(started(&plan_with(minutes(5))), ["o/small"]);
    let held = plan_with(minutes(6));
    assert_eq!(started(&held), [] as [&str; 0]);
    assert_eq!(held.waiting[&6], Waiting::BehindMemory { job: 5 });
}

/// Waiting for a slot only, an older job has its room now: taken, it would not start when the slot frees.
#[test]
fn a_younger_job_ends_before_an_older_job_gets_its_slot() {
    let plan_with = |usual| {
        let in_older = Slot::Runner(RunnerSlot {
            repository: "o/older".to_owned(),
            busy: true,
            started_at: now() - BEFORE,
            memory: 2 * GIB,
            cpu: 0,
            usual: Some(BEFORE + minutes(5)),
            caps: Caps {
                memory: 2 * GIB,
                cpu: 0,
            },
            behind: None,
        });
        let mut profiles = slots(vec![running(10 * GIB, minutes(20))]);
        profiles.insert("older".to_owned(), profile(2 * GIB, 1000, vec![in_older]));
        let owner = Repository {
            waits: starved(5),
            ..pool("o/big", "big", 8 * GIB, None, &[5])
        };
        plan(
            &[
                pool("o/older", "older", 2 * GIB, None, &[1]),
                owner,
                pool("o/small", "small", GIB, Some(usual), &[6]),
            ],
            &profiles,
            slice(12 * GIB),
            now(),
        )
    };
    assert_eq!(started(&plan_with(minutes(5))), ["o/small"]);
    assert_eq!(started(&plan_with(minutes(6))), [] as [&str; 0]);
}

/// A job no running runner's end makes room for starts behind nothing: it must not stop every job behind the barrier.
#[test]
fn an_older_job_no_end_makes_room_for_holds_nothing_back() {
    let too_big = pool("o/older", "older", 40 * GIB, None, &[1]);
    let owner = Repository {
        waits: starved(5),
        ..pool("o/big", "big", 8 * GIB, None, &[5])
    };
    let mut profiles = slots(room_in_twenty_minutes());
    profiles.insert(
        "older".to_owned(),
        profile(40 * GIB, 1000, vec![Slot::Free]),
    );
    let plan = plan(
        &[
            too_big,
            owner,
            pool("o/small", "small", GIB, Some(minutes(1)), &[6]),
        ],
        &profiles,
        slice(12 * GIB),
        now(),
    );
    assert_eq!(started(&plan), ["o/small"]);
}

/// A preparing profile has no free slot and no runner ending, but its preparation ends on its own, and the older job
/// then takes the room it has now.
#[test]
fn a_younger_job_leaves_the_room_of_an_older_job_whose_profile_is_preparing() {
    let mut profiles = slots(vec![
        running(3 * GIB, minutes(20)),
        running(6 * GIB, minutes(40)),
    ]);
    let reseeding = Profile {
        preparing: Some(Preparing::Reseeding),
        ..profile(6 * GIB, 1000, vec![Slot::Free])
    };
    profiles.insert("older".to_owned(), reseeding);
    let plan = plan(
        &[
            pool("o/older", "older", 6 * GIB, None, &[0]),
            owner(),
            pool("o/small", "small", 2 * GIB, Some(minutes(15)), &[2]),
        ],
        &profiles,
        slice(9 * GIB),
        now(),
    );
    assert_eq!(started(&plan), [] as [&str; 0]);
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

/// An older job that matures takes the barrier, and what started behind the first must leave it its room as well, or a
/// hung job keeps it, and the owner behind it, out.
#[test]
fn the_caps_leave_an_older_jobs_room_as_they_leave_the_owners() {
    let plan_with = |cap| {
        let needs_little = Repository {
            waits: starved(1),
            ..pool("o/big", "big", 4 * GIB, None, &[1])
        };
        let mut profiles = slots(vec![
            running(3 * GIB, minutes(20)),
            running(10 * GIB, minutes(40)),
        ]);
        profiles.insert(
            "older".to_owned(),
            profile(14 * GIB, 1000, vec![Slot::Free]),
        );
        profiles.insert("wide".to_owned(), profile(cap, 1000, vec![Slot::Free; 2]));
        plan(
            &[
                pool("o/older", "older", 14 * GIB, None, &[0]),
                needs_little,
                pool("o/wide", "wide", GIB, Some(minutes(10)), &[2]),
            ],
            &profiles,
            slice(13 * GIB),
            now(),
        )
    };
    assert_eq!(started(&plan_with(2 * GIB)), ["o/wide"]);
    assert_eq!(started(&plan_with(3 * GIB)), [] as [&str; 0]);
}

/// All of a pool's runners take the same, so what does not fit the owner does not fit its younger jobs.
#[test]
fn the_owners_own_younger_jobs_never_pass_it() {
    let repositories = [Repository {
        usual: Some(minutes(1)),
        waits: starved(1),
        ..pool("o/big", "big", 8 * GIB, None, &[1, 2])
    }];
    let plan = plan(
        &repositories,
        &slots(room_in_twenty_minutes()),
        slice(12 * GIB),
        now(),
    );
    assert_eq!(started(&plan), [] as [&str; 0]);
    // It would pass by its run time, so what it lacks is what holds it, as it holds the owner.
    assert!(
        matches!(plan.waiting[&2], Waiting::Memory { .. }),
        "{plan:?}"
    );
}

/// A backfill that hangs in the owner's last free slot would hold it however much memory drains.
#[test]
fn a_younger_job_of_the_owners_profile_leaves_it_a_slot() {
    let in_its_profile = pool("o/sibling", "big", GIB, Some(minutes(1)), &[2, 3]);
    let plan = plan(
        &[owner(), in_its_profile],
        &slots(room_in_twenty_minutes()),
        Budget {
            drained: Drained {
                memory: Some(gib(32)),
                ..Drained::default()
            },
            ..slice(12 * GIB)
        },
        now(),
    );
    assert_eq!(started(&plan), ["o/sibling"], "one of the two slots stays");
    assert_eq!(plan.waiting[&3], Waiting::BehindMemory { job: 1 });
}

#[test]
fn two_pools_of_one_repository_go_by_their_own_run_times() {
    let quick = Repository {
        labels: vec!["quick".to_owned()],
        ..pool("o/both", "small", GIB, Some(minutes(1)), &[])
    };
    let slow = Repository {
        labels: vec!["slow".to_owned()],
        profile: "other".to_owned(),
        ..pool("o/both", "small", GIB, Some(minutes(60)), &[])
    };
    let queued = vec![job(2, &["quick"]), job(3, &["slow"])];
    let repositories = [
        owner(),
        Repository {
            queued: queued.clone(),
            ..quick
        },
        Repository { queued, ..slow },
    ];
    let mut profiles = slots(room_in_twenty_minutes());
    profiles.get_mut("other").unwrap().slots.push(Slot::Free);
    profiles.get_mut("other").unwrap().count += 1;
    let plan = plan(&repositories, &profiles, slice(12 * GIB), now());
    assert_eq!(started(&plan), ["o/both"]);
    assert!(
        matches!(
            &plan.decisions[..],
            [Decision::Start { slot, .. }] if slot.profile == "small"
        ),
        "the quick pool passes: {plan:?}"
    );
    assert_eq!(plan.waiting[&3], Waiting::BehindMemory { job: 1 });
}

/// After a clock that went back a runner looks younger than it is: it must not look longer to run than it usually does.
#[test]
fn a_clock_that_went_back_makes_no_runner_end_later_than_its_usual_run_time() {
    let from_the_future = |ends_in| {
        Slot::Runner(RunnerSlot {
            repository: "o/other".to_owned(),
            busy: true,
            started_at: now() + minutes(60),
            memory: 4 * GIB,
            cpu: 0,
            usual: Some(ends_in),
            caps: Caps {
                memory: 4 * GIB,
                cpu: 0,
            },
            behind: None,
        })
    };
    let runners = || vec![from_the_future(minutes(5)), running(8 * GIB, minutes(40))];
    let held = plan_behind(younger(Some(minutes(6))), runners());
    assert_held(&held);
    // Its whole usual run time is not what it has left: the room it gives comes now, so nothing passes.
    let matured_later = Repository {
        waits: Waits::default().with_memory(Some(Wait {
            job: 1,
            since: now() - WAITED,
        })),
        ..owner()
    };
    let in_the_past_for_the_barrier = Slot::Runner(RunnerSlot {
        repository: "o/other".to_owned(),
        busy: true,
        started_at: now() + minutes(1),
        memory: 7 * GIB,
        cpu: 0,
        usual: Some(minutes(30)),
        caps: Caps {
            memory: 7 * GIB,
            cpu: 0,
        },
        behind: None,
    });
    let mut wide = slots(vec![
        in_the_past_for_the_barrier,
        running(8 * GIB, minutes(40)),
    ]);
    wide.get_mut("other").unwrap().memory = GIB;
    let plan_of = plan(
        &[matured_later, younger(Some(minutes(20)))],
        &wide,
        slice(15 * GIB),
        now(),
    );
    assert_eq!(started(&plan_of), [] as [&str; 0]);
    // Started "later" than the barrier matured, so its cap counts as well: 8G, all a drain leaves past the owner.
    let wait_from_the_future = Repository {
        waits: Waits::default().with_memory(Some(Wait {
            job: 1,
            since: now() + minutes(1),
        })),
        ..owner()
    };
    let plan = plan(
        &[wait_from_the_future, younger(Some(minutes(1)))],
        &slots(room_in_twenty_minutes()),
        slice(12 * GIB),
        now(),
    );
    assert_eq!(plan.barrier, None, "a wait from the future has not matured");
}

/// A pooled count can bring the owner's room with no runner ending: its runner shrinks, and the credit grows.
#[test]
fn a_credit_that_may_come_before_any_end_holds_back_what_would_outlast_it() {
    // 12G of 16G in use: 40 minutes to the room of the 8G owner by ends alone.
    let runners = vec![running(12 * GIB, minutes(40))];
    let behind = |credit_room| {
        plan(
            &[owner(), younger(Some(minutes(30)))],
            &slots(runners.clone()),
            Budget {
                credit_room,
                ..slice(12 * GIB)
            },
            now(),
        )
    };
    assert_passes(&behind(0));
    // Short of the owner's room however it grows: the end still decides.
    assert_passes(&behind(3 * GIB));
    assert_held(&behind(4 * GIB));
    assert_held(&behind(6 * GIB));
}

/// The owner's status at 14:22:38 UTC on 2026-11-10, in GiB: fretboard's `check:backend:static` (jvm-heavy, 11G)
/// matured at 14:22:03, and since then three runners started for older jvm and node jobs, which GitHub gave younger
/// jobs of the same pools. Before rule 4 counted only what passed, their caps (13.25 + 9.75 + 9.75) left a drain of
/// 39.81G less the owner's 11G short by 3.94G, and no light job passed.
mod at_14_22_38 {
    use super::*;

    const UP_SINCE_T0: Duration = Duration::from_secs(35);

    fn gib_of(hundredths: u64) -> u64 {
        GIB * hundredths / 100
    }

    /// A runner of fretboard in `profile`, `up` seconds old, taking `hundredths` of a GiB, that usually ends in
    /// `ends_in`.
    fn runner(up: u64, hundredths: u64, ends_in: Duration) -> Slot {
        let up = Duration::from_secs(up);
        Slot::Runner(RunnerSlot {
            repository: "fretboard".to_owned(),
            busy: true,
            started_at: now() - up,
            memory: gib_of(hundredths),
            cpu: 0,
            usual: Some(up + ends_in),
            caps: Caps {
                memory: gib_of(hundredths),
                cpu: 0,
            },
            behind: None,
        })
    }

    /// The profiles at 14:22:38: what each runner took, how long it had run, and whether it started after T0.
    fn profiles(light: Vec<Slot>) -> BTreeMap<String, Profile> {
        let jvm = vec![
            runner(20, 700, minutes(6)),
            runner(39, 375, minutes(4)),
            runner(52, 400, minutes(3)),
            Slot::Free,
            Slot::Free,
            Slot::Free,
        ];
        let node = vec![
            runner(148, 300, minutes(3)),
            runner(240, 300, minutes(3)),
            runner(333, 300, minutes(2)),
            runner(21, 425, minutes(5)),
            runner(40, 300, minutes(5)),
            runner(200, 300, minutes(4)),
            runner(413, 300, minutes(2)),
            runner(33, 150, minutes(2)),
        ];
        BTreeMap::from([
            (
                "jvm-heavy".to_owned(),
                profile(11 * GIB, 0, vec![Slot::Free; 3]),
            ),
            ("jvm".to_owned(), profile(gib_of(1325), 0, jvm)),
            ("node".to_owned(), profile(gib_of(975), 0, node)),
            ("light".to_owned(), profile(gib_of(175), 0, light)),
        ])
    }

    /// The owner, job 10, and an older job each of jvm (7G) and node (5G), whose pools a start served since; then a
    /// light job, 0.5G, that runs 30 s.
    fn pools(light: &[u64]) -> Vec<Repository> {
        let owner = Repository {
            waits: Waits::default().with_memory(Some(Wait {
                job: 10,
                since: now() - UP_SINCE_T0 - PRIORITY_AFTER,
            })),
            ..pool("fretboard", "jvm-heavy", 11 * GIB, Some(minutes(3)), &[10])
        };
        vec![
            pool("fretboard", "jvm", 7 * GIB, Some(minutes(6)), &[3]),
            pool("fretboard", "node", 5 * GIB, Some(minutes(5)), &[4]),
            owner,
            pool(
                "fretboard",
                "light",
                GIB / 2,
                Some(Duration::from_secs(30)),
                light,
            ),
        ]
    }

    /// The host left the slice 39.81G, of which the runners took 39.00G.
    fn budget(used: u64) -> Budget {
        let drain = gib_of(3981);
        Budget {
            memory: Some(super::super::super::left(drain, used)),
            drained: Drained {
                memory: Some(super::super::super::left(drain, 0)),
                ..Drained::default()
            },
            ..Budget::default()
        }
    }

    #[test]
    fn a_light_job_passes_a_heavy_owner_behind_runners_started_for_older_jobs() {
        let light = vec![
            runner(47, 50, minutes(1)),
            Slot::Free,
            Slot::Free,
            Slot::Free,
        ];
        let plan = plan(&pools(&[20]), &profiles(light), budget(gib_of(3900)), now());
        assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(10));
        let started: Vec<&str> = plan
            .decisions
            .iter()
            .filter_map(|decision| match decision {
                Decision::Start { slot, behind, .. } => {
                    assert_eq!(*behind, Some(10), "it passed the owner");
                    Some(slot.profile.as_str())
                }
                Decision::Stop { .. } => None,
            })
            .collect();
        assert_eq!(started, ["light"], "{plan:?}");
    }

    /// The light runner that passed takes 0.5G and has not ended: once the older jobs' runners and those from before
    /// T0 have, the owner fits beside it.
    #[test]
    fn the_owner_starts_once_the_older_runners_end() {
        let passed = Slot::Runner(RunnerSlot {
            repository: "fretboard".to_owned(),
            busy: true,
            started_at: now() - UP_SINCE_T0 / 2,
            memory: GIB / 2,
            cpu: 0,
            usual: Some(Duration::from_secs(30)),
            caps: Caps {
                memory: gib_of(175),
                cpu: 0,
            },
            behind: Some(10),
        });
        let mut profiles = profiles(vec![passed, Slot::Free, Slot::Free, Slot::Free]);
        for name in ["jvm", "node"] {
            let profile = profiles.get_mut(name).unwrap();
            profile.slots.fill(Slot::Free);
        }
        let owner_alone = &pools(&[])[2..3];
        let plan = plan(owner_alone, &profiles, budget(GIB / 2), now());
        let started: Vec<&str> = plan
            .decisions
            .iter()
            .filter_map(|decision| match decision {
                Decision::Start { slot, .. } => Some(slot.profile.as_str()),
                Decision::Stop { .. } => None,
            })
            .collect();
        assert_eq!(started, ["jvm-heavy"], "{plan:?}");
    }
}

/// The planner, a model of the engine and of GitHub: jobs queue, runners take the oldest job of their pool and run as
/// long as a run time says, and nothing but the plan decides. Run times here are the model's own.
mod generated {
    use std::sync::Mutex;

    use super::*;

    /// A pool of the model: what a runner takes of the budgets, and what its cap lets it grow to.
    #[derive(Debug, Clone, Copy)]
    struct Kind {
        repository: &'static str,
        profile: &'static str,
        memory: u64,
        cap: u64,
        cpu: u64,
    }

    /// A pooled count's credit in a model: it comes and goes with no runner ending (ADR 0099).
    #[derive(Debug, Clone, Copy)]
    struct Credit {
        /// The first tick it stands.
        from: u64,
        /// The tick it is gone by.
        until: u64,
        /// The most it may be; else half of what the runners older than this tick count, so a runner just started
        /// changes nothing and one that ends takes back less than it gives.
        most: u64,
    }

    /// The limits of a model.
    #[derive(Debug, Clone, Copy)]
    struct Shape {
        /// How long a tick is, which says how many a job waits before it holds: `PRIORITY_AFTER`, or
        /// `SERVED_PRIORITY_AFTER` once a barrier served its pool.
        tick: Duration,
        /// How many ticks apart a small world's jobs may arrive.
        gap: usize,
        memory: u64,
        cpu: Option<u64>,
        slots: usize,
        pools: [Kind; 3],
        /// When a job that hangs has grown to its cap: late enough that the budgets gave no warning while jobs passed,
        /// which only rule 4 of ADRs 0097 and 0131 stands against.
        hung_jobs_grow_at: u64,
        credit: Option<Credit>,
    }

    /// Long runs of many jobs: slots to spare, memory short.
    static STREAMS: Shape = Shape {
        tick: Duration::from_secs(150),
        gap: 1,
        memory: 16 * GIB,
        cpu: None,
        slots: 16,
        pools: [
            Kind {
                repository: "o/big",
                profile: "big",
                memory: 8 * GIB,
                cap: 8 * GIB,
                cpu: 0,
            },
            Kind {
                repository: "o/mid",
                profile: "mid",
                memory: 3 * GIB,
                cap: 3 * GIB,
                cpu: 0,
            },
            Kind {
                repository: "o/small",
                profile: "small",
                memory: GIB,
                cap: 2 * GIB,
                cpu: 0,
            },
        ],
        hung_jobs_grow_at: 30,
        credit: None,
    };

    /// `STREAMS` with a big job of more than half the slice: until the room gathers, what passes and hangs behind its
    /// barrier can take more than a drain leaves past it, which only rule 4 of ADRs 0097 and 0131 stands against.
    static HEAVY_STREAMS: Shape = Shape {
        pools: [
            Kind {
                memory: 10 * GIB,
                cap: 10 * GIB,
                ..STREAMS.pools[0]
            },
            STREAMS.pools[1],
            STREAMS.pools[2],
        ],
        ..STREAMS
    };

    /// A few jobs in two slots a pool, a job that waits a tick being starved: memory, CPU and slots each run out.
    static TIGHT: Shape = Shape {
        tick: Duration::from_secs(600),
        gap: 1,
        memory: 8 * GIB,
        cpu: Some(4000),
        slots: 2,
        pools: [
            Kind {
                repository: "o/big",
                profile: "big",
                memory: 6 * GIB,
                cap: 6 * GIB,
                cpu: 2000,
            },
            Kind {
                repository: "o/mid",
                profile: "mid",
                memory: 3 * GIB,
                cap: 3 * GIB,
                cpu: 2000,
            },
            Kind {
                repository: "o/small",
                profile: "small",
                memory: GIB,
                cap: 2 * GIB,
                cpu: 1000,
            },
        ],
        hung_jobs_grow_at: 6,
        credit: None,
    };

    /// Two pools of one profile, its caps leaving room to pass, and two slots: a younger pool could take them both.
    static SHARED: Shape = Shape {
        tick: Duration::from_secs(600),
        gap: 1,
        memory: 11 * GIB,
        cpu: None,
        slots: 2,
        pools: [
            Kind {
                repository: "o/wide",
                profile: "shared",
                memory: 3 * GIB,
                cap: 3 * GIB,
                cpu: 0,
            },
            Kind {
                repository: "o/thin",
                profile: "shared",
                memory: GIB,
                cap: 3 * GIB,
                cpu: 0,
            },
            Kind {
                repository: "o/other",
                profile: "other",
                memory: 9 * GIB,
                cap: 9 * GIB,
                cpu: 0,
            },
        ],
        hung_jobs_grow_at: 6,
        credit: None,
    };

    /// `TIGHT` with the two limits apart: a job waits two ticks, or one once a barrier served its pool, and a job can
    /// arrive behind a barrier that went up since the last.
    static TWO_LIMITS: Shape = Shape {
        tick: Duration::from_secs(300),
        gap: 2,
        memory: 8 * GIB,
        cpu: Some(4000),
        slots: 2,
        pools: TIGHT.pools,
        hung_jobs_grow_at: 6,
        credit: None,
    };

    /// `SplitMix64`: the same seed gives the same sequence on every machine.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct Job {
        id: u64,
        pool: usize,
        /// How many ticks it really runs.
        ticks: u64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Estimates {
        /// The planner knows none: a strict barrier.
        None,
        /// The planner reads each job's real run time.
        Exact,
        /// The planner reads twice each job's real run time, as a p95 reads a run that ends at its median.
        Double,
        /// The planner reads half each job's real run time, rounded up: runners run past their usual end, so the room
        /// may come at any moment as far as the plan knows, and only short jobs pass (ADR 0151).
        Half,
    }

    /// What a run does.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Runs {
        /// It ends on time.
        OnTime,
        /// A job that started behind a barrier never ends; the rest do.
        BackfilledHang,
    }

    #[derive(Debug, Clone)]
    struct Run {
        job: Job,
        started: u64,
        /// `None`: it hangs.
        ends: Option<u64>,
        /// The barrier it started behind.
        behind: Option<u64>,
    }

    #[derive(Clone)]
    struct World {
        shape: &'static Shape,
        tick: u64,
        estimates: Estimates,
        runs: Runs,
        queued: Vec<Job>,
        waits: [Waits; 3],
        running: Vec<Run>,
        /// Each job's start, by tick.
        starts: BTreeMap<u64, u64>,
        /// The jobs that owned a barrier, in the order they first did.
        owners: Vec<u64>,
        passed: u64,
        /// Up to which tick a runner that did not start behind a barrier ran.
        last_end: u64,
        /// Up to which tick a runner that started behind a barrier ran.
        last_pass_end: u64,
    }

    /// What became of an owner, from the plan that first put up its barrier.
    struct Outcome {
        start: u64,
        /// Whether it owned every barrier until it started: an older job that matures takes the barrier from it.
        kept: bool,
        last_pass_end: u64,
    }

    impl World {
        fn new(shape: &'static Shape, estimates: Estimates, runs: Runs) -> World {
            World {
                shape,
                tick: 0,
                estimates,
                runs,
                queued: Vec::new(),
                waits: [Waits::default(); 3],
                running: Vec::new(),
                starts: BTreeMap::new(),
                owners: Vec::new(),
                passed: 0,
                last_end: 0,
                last_pass_end: 0,
            }
        }

        /// The same world from here on, reading `estimates`.
        fn fork(&self, estimates: Estimates) -> World {
            World {
                estimates,
                last_pass_end: 0,
                ..self.clone()
            }
        }

        /// Runs this world until `owner` starts.
        fn owner_outcome(mut self, owner: u64, arrivals: &[Vec<Job>]) -> Outcome {
            let mut kept = true;
            loop {
                let plan = self.step(arrival_at(arrivals, self.tick));
                if let Some(start) = self.starts.get(&owner) {
                    return Outcome {
                        start: *start,
                        kept,
                        last_pass_end: self.last_pass_end,
                    };
                }
                kept &= plan.barrier.is_some_and(|barrier| barrier.job == owner);
            }
        }

        /// The daemon restarts: every wait goes, with the served marks, and the runners it adopts did not pass a barrier
        /// as far as it knows (ADR 0131). They are what a barrier that matures after it waits out.
        fn restart(&mut self) {
            self.waits = [Waits::default(); 3];
            for run in &mut self.running {
                if run.behind.take().is_some() {
                    let ends = run.ends.unwrap_or(u64::MAX);
                    self.last_end = self.last_end.max(ends);
                }
            }
        }

        /// Whether nothing is left to arrive, wait or run.
        fn done(&self, arrivals: &[Vec<Job>]) -> bool {
            usize::try_from(self.tick).unwrap() >= arrivals.len()
                && self.queued.is_empty()
                && self.running.is_empty()
        }

        fn at(&self, tick: u64) -> SystemTime {
            now() + self.shape.tick * u32::try_from(tick).unwrap()
        }

        fn estimate(&self, ticks: u64) -> Option<Duration> {
            let read = self.shape.tick * u32::try_from(ticks).unwrap();
            match self.estimates {
                Estimates::None => None,
                Estimates::Exact => Some(read),
                Estimates::Double => Some(read * 2),
                Estimates::Half => {
                    Some(self.shape.tick * u32::try_from(ticks.div_ceil(2)).unwrap())
                }
            }
        }

        /// What `run` takes of the memory budget now.
        fn takes(&self, run: &Run) -> u64 {
            let kind = self.shape.pools[run.job.pool];
            if run.ends.is_none() && self.tick >= self.shape.hung_jobs_grow_at {
                kind.cap
            } else {
                kind.memory
            }
        }

        fn repositories(&self) -> Vec<Repository> {
            (0..self.shape.pools.len())
                .map(|index| {
                    let kind = self.shape.pools[index];
                    let jobs: Vec<&Job> =
                        self.queued.iter().filter(|job| job.pool == index).collect();
                    let longest = jobs.iter().map(|job| job.ticks).max();
                    let ids: Vec<u64> = jobs.iter().map(|job| job.id).collect();
                    let usual = longest.and_then(|ticks| self.estimate(ticks));
                    Repository {
                        waits: self.waits[index],
                        runner_cpu: Some(kind.cpu),
                        ..pool(kind.repository, kind.profile, kind.memory, usual, &ids)
                    }
                })
                .collect()
        }

        fn profiles(&self) -> BTreeMap<String, Profile> {
            let mut profiles = BTreeMap::new();
            for kind in &self.shape.pools {
                let mut slots: Vec<Slot> = self
                    .running
                    .iter()
                    .filter(|run| self.shape.pools[run.job.pool].profile == kind.profile)
                    .map(|run| {
                        let of_run = self.shape.pools[run.job.pool];
                        Slot::Runner(RunnerSlot {
                            repository: of_run.repository.to_owned(),
                            busy: true,
                            started_at: self.at(run.started),
                            memory: self.takes(run),
                            cpu: of_run.cpu,
                            usual: self.estimate(run.job.ticks),
                            caps: Caps {
                                memory: kind.cap,
                                cpu: kind.cpu,
                            },
                            behind: run.behind,
                        })
                    })
                    .collect();
                slots.resize(self.shape.slots.max(slots.len()), Slot::Free);
                profiles.insert(kind.profile.to_owned(), profile(kind.cap, kind.cpu, slots));
            }
            profiles
        }

        /// What the count takes off now.
        fn credit(&self) -> u64 {
            let Some(credit) = self.shape.credit else {
                return 0;
            };
            if !(credit.from..credit.until).contains(&self.tick) {
                return 0;
            }
            let settled = self.running.iter().filter(|run| run.started < self.tick);
            let held: u64 = settled.map(|run| self.takes(run)).sum();
            credit.most.min(held / 2)
        }

        fn budget(&self) -> Budget {
            let left = crate::scheduler::left;
            let memory: u64 = self.running.iter().map(|run| self.takes(run)).sum();
            let credit = self.credit();
            let kinds = &self.shape.pools;
            let cpu: u64 = self.running.iter().map(|run| kinds[run.job.pool].cpu).sum();
            Budget {
                memory: Some(left(self.shape.memory, memory - credit)),
                cpu: self.shape.cpu.map(|budget| left(budget, cpu)),
                credit_room: self.shape.credit.map_or(0, |most| most.most - credit),
                drained: Drained {
                    memory: Some(left(self.shape.memory, 0)),
                    cpu: self.shape.cpu.map(|budget| left(budget, 0)),
                    ..Drained::default()
                },
                ..Budget::default()
            }
        }

        fn index_of(&self, repository: &str) -> usize {
            let pools = &self.shape.pools;
            pools
                .iter()
                .position(|kind| kind.repository == repository)
                .unwrap()
        }

        /// One tick: what is due ends, `arrivals` queue, and one plan is carried out as the engine would.
        fn step(&mut self, arrivals: &[Job]) -> Plan {
            let tick = self.tick;
            self.running
                .retain(|run| run.ends.is_none_or(|ends| ends > tick));
            self.queued.extend_from_slice(arrivals);
            let at = self.at(tick);
            let plan = plan(&self.repositories(), &self.profiles(), self.budget(), at);
            for decision in &plan.decisions {
                let Decision::Start {
                    repository, behind, ..
                } = decision
                else {
                    continue;
                };
                let index = self.index_of(repository);
                let oldest = self
                    .queued
                    .iter()
                    .position(|job| job.pool == index)
                    .unwrap();
                let job = self.queued.remove(oldest);
                self.waits[index].started();
                self.starts.insert(job.id, tick);
                let end = tick + job.ticks;
                if behind.is_some() {
                    self.passed += 1;
                    self.last_pass_end = self.last_pass_end.max(end);
                } else {
                    self.last_end = self.last_end.max(end);
                }
                let hangs = behind.is_some() && self.runs == Runs::BackfilledHang;
                self.running.push(Run {
                    job,
                    started: tick,
                    ends: (!hangs).then_some(end),
                    behind: *behind,
                });
            }
            for demand in &plan.held {
                let index = self.index_of(&demand.pool.repository);
                self.waits[index].note(demand.scarce, demand.job, at);
            }
            for index in 0..self.waits.len() {
                if !self.queued.iter().any(|job| job.pool == index) {
                    self.waits[index].emptied();
                }
                if let Some(barrier) = plan.barrier {
                    self.waits[index].own(barrier.job);
                }
            }
            if let Some(barrier) = plan.barrier
                && !self.owners.contains(&barrier.job)
            {
                self.owners.push(barrier.job);
            }
            self.tick += 1;
            plan
        }
    }

    /// Small and mid jobs filling the slice, a big job among them, and more behind it, tick by tick.
    fn arrivals(rng: &mut Rng) -> Vec<Vec<Job>> {
        let mut next = 1;
        let mut job = |pool, ticks| {
            let id = next;
            next += 1;
            Job { id, pool, ticks }
        };
        let mut ticks: Vec<Vec<Job>> = Vec::new();
        for tick in 0..120 {
            let mut now = Vec::new();
            for _ in 0..rng.below(3) {
                let (pool, longest) =
                    [(1, 30), (2, 6), (2, 6)][usize::try_from(rng.below(3)).unwrap()];
                now.push(job(pool, 1 + rng.below(longest)));
            }
            if tick % 40 == 5 {
                now.push(job(0, 1 + rng.below(10)));
            }
            ticks.push(now);
        }
        ticks
    }

    fn arrival_at(arrivals: &[Vec<Job>], tick: u64) -> &[Job] {
        let at = usize::try_from(tick).unwrap();
        arrivals.get(at).map_or(&[], Vec::as_slice)
    }

    /// What the comparisons at every barrier of some worlds came to.
    #[derive(Debug, Default)]
    struct Tally {
        worlds: u64,
        owners: u64,
        /// Of those, how many owned every barrier until they started, in both worlds compared.
        kept: u64,
        passed: u64,
        /// An owner that kept its barrier and started later than under a strict one: what bound 1 rules out.
        late: Vec<String>,
        /// How many that lost their barrier started later, and by how many ticks at most: what ADR 0097 says can happen.
        /// On every small world so far, only with estimates that read long.
        lost_and_late: u64,
        most_late_after_losing: u64,
        /// An owner that kept its barrier and started later than strict and than the end of all that passed it.
        past_the_ceiling: Vec<String>,
        /// How many owners that kept their barrier started later for estimates that read long, and by how much at most.
        read_long_and_late: u64,
        most_late_read_long: u64,
        /// An owner a hung job kept out once all else had ended: what bound 2 rules out.
        kept_out: Vec<String>,
        hung: u64,
    }

    impl Tally {
        fn add(&mut self, other: Tally) {
            self.worlds += other.worlds;
            self.owners += other.owners;
            self.kept += other.kept;
            self.passed += other.passed;
            self.late.extend(other.late);
            self.lost_and_late += other.lost_and_late;
            self.most_late_after_losing = self
                .most_late_after_losing
                .max(other.most_late_after_losing);
            self.past_the_ceiling.extend(other.past_the_ceiling);
            self.read_long_and_late += other.read_long_and_late;
            self.most_late_read_long = self.most_late_read_long.max(other.most_late_read_long);
            self.kept_out.extend(other.kept_out);
            self.hung += other.hung;
        }

        /// Compares, at every barrier of the world of `arrivals`, what comes of its owner with the jobs behind it
        /// starting and with none: from the same state, before the plan that put the barrier up, as that plan offers
        /// the most room. A world in which nothing passes is its own strict world.
        fn compare(
            &mut self,
            shape: &'static Shape,
            read: Estimates,
            arrivals: &[Vec<Job>],
            name: &dyn Fn() -> String,
        ) {
            let mut passing = World::new(shape, read, Runs::OnTime);
            while !passing.done(arrivals) {
                passing.step(arrival_at(arrivals, passing.tick));
            }
            self.passed += passing.passed;
            if passing.passed == 0 {
                self.owners += passing.owners.len() as u64;
                self.kept += passing.owners.len() as u64;
                return;
            }
            let mut world = World::new(shape, read, Runs::OnTime);
            let mut seen = 0;
            while !world.done(arrivals) {
                let before = world.clone();
                world.step(arrival_at(arrivals, world.tick));
                for owner in world.owners[seen..].iter().copied() {
                    self.owners += 1;
                    let strict = before.fork(Estimates::None).owner_outcome(owner, arrivals);
                    let passing = before.fork(read).owner_outcome(owner, arrivals);
                    let late = passing.start.saturating_sub(strict.start);
                    let kept = strict.kept && passing.kept;
                    self.kept += u64::from(kept);
                    let what = || {
                        format!(
                            "{}: owner {owner} at tick {}: strict {}, with passes {}, the last ending at {}",
                            name(),
                            before.tick,
                            strict.start,
                            passing.start,
                            passing.last_pass_end
                        )
                    };
                    if late > 0 && !kept {
                        self.lost_and_late += 1;
                        self.most_late_after_losing = self.most_late_after_losing.max(late);
                    } else if late > 0 && read == Estimates::Exact {
                        self.late.push(what());
                    } else if late > 0 {
                        self.read_long_and_late += 1;
                        self.most_late_read_long = self.most_late_read_long.max(late);
                        if passing.start > strict.start.max(passing.last_pass_end) {
                            self.past_the_ceiling.push(what());
                        }
                    }
                }
                seen = world.owners.len();
            }
        }

        /// Hangs every job that starts behind a barrier and checks the first barrier's owner, if it keeps that barrier.
        fn hang(&mut self, shape: &'static Shape, arrivals: &[Vec<Job>]) {
            let mut world = World::new(shape, Estimates::Exact, Runs::BackfilledHang);
            let mut owner = None;
            for tick in 0..200 {
                let plan = world.step(arrival_at(arrivals, tick));
                if let Some(owner) = owner
                    && world.starts.contains_key(&owner)
                {
                    self.hung += u64::from(world.passed > 0);
                    return;
                }
                let barrier = plan.barrier.map(|barrier| barrier.job);
                match (owner, barrier) {
                    (None, Some(job)) => owner = Some(job),
                    // An older job took the barrier: the bound was the first owner's while it stood.
                    (Some(owner), Some(job)) if job < owner => return,
                    _ => {}
                }
            }
            if let Some(owner) = owner {
                self.kept_out.push(format!(
                    "{arrivals:?}: owner {owner}, the last job not behind a barrier ending at {}",
                    world.last_end
                ));
            }
        }
    }

    /// The first bound of ADR 0097, over long generated runs.
    #[test]
    fn with_run_times_that_hold_an_owner_that_keeps_its_barrier_starts_as_under_a_strict_one() {
        let mut tally = Tally::default();
        for seed in 0..128 {
            let arrivals = arrivals(&mut Rng(seed));
            tally.compare(&STREAMS, Estimates::Exact, &arrivals, &|| {
                format!("seed {seed}")
            });
        }
        assert_eq!(tally.late, [] as [String; 0]);
        assert!(
            tally.owners > 100,
            "generated runs reach barriers: {tally:?}"
        );
        assert!(tally.passed > 20, "and younger jobs pass them: {tally:?}");
    }

    /// Run times that read long put the owner's room too late, so it can come while jobs still run behind the barrier.
    /// What they cost it stops at their ends.
    #[test]
    fn with_run_times_that_read_long_an_owner_waits_no_longer_than_what_passed_it() {
        let mut tally = Tally::default();
        for seed in 0..128 {
            let arrivals = arrivals(&mut Rng(seed));
            tally.compare(&STREAMS, Estimates::Double, &arrivals, &|| {
                format!("seed {seed}")
            });
        }
        assert_eq!(tally.past_the_ceiling, [] as [String; 0]);
        assert!(
            tally.read_long_and_late > 0,
            "the ceiling is reached for: {tally:?}"
        );
    }

    /// `STREAMS` in minute ticks, so a job of one or two ticks is short (`SHORT_RUN`).
    static MINUTE_STREAMS: Shape = Shape {
        tick: Duration::from_secs(60),
        ..STREAMS
    };

    /// With run times that read short, runners run past their usual end and no time is known for the owner's room, so
    /// only short jobs pass (ADR 0151). Each costs the owner at most what it has left to run once the room comes: no owner
    /// starts past the later of its strict start and the end of what passed it, nor more than `SHORT_RUN` after its
    /// strict start.
    #[test]
    fn with_run_times_that_read_short_an_owner_waits_at_most_a_short_run_longer() {
        let mut tally = Tally::default();
        for seed in 0..48 {
            let arrivals = arrivals(&mut Rng(seed));
            tally.compare(&MINUTE_STREAMS, Estimates::Half, &arrivals, &|| {
                format!("seed {seed}")
            });
        }
        assert_eq!(tally.past_the_ceiling, [] as [String; 0]);
        let short_run = SHORT_RUN.as_secs() / MINUTE_STREAMS.tick.as_secs();
        assert!(
            tally.most_late_read_long <= short_run,
            "at most a short run late: {tally:?}"
        );
        assert!(tally.passed > 50, "short jobs pass: {tally:?}");
    }

    /// Five mid runners filling the slice and ending one by one, a big job behind them, and a stream of short small jobs.
    fn a_stream_behind_a_big_job(rng: &mut Rng) -> Vec<Vec<Job>> {
        let mut ticks = vec![Vec::new(); 80];
        for id in 1..=5 {
            let usual = 10 * id + rng.below(10);
            ticks[0].push(Job {
                id,
                pool: 1,
                ticks: usual,
            });
        }
        ticks[1].push(Job {
            id: 6,
            pool: 0,
            ticks: 5,
        });
        for (tick, id) in (2..80).zip(7..) {
            let usual = 1 + rng.below(3);
            ticks[tick].push(Job {
                id,
                pool: 2,
                ticks: usual,
            });
        }
        ticks
    }

    /// The second bound of ADR 0097: whatever started behind the barrier does, the owner fits once the rest ended.
    #[test]
    fn jobs_behind_the_barrier_that_never_end_cannot_keep_the_owner_out() {
        let mut most = 0;
        for seed in 0..64 {
            let arrivals = a_stream_behind_a_big_job(&mut Rng(seed));
            let mut world = World::new(&HEAVY_STREAMS, Estimates::Exact, Runs::BackfilledHang);
            for tick in 0..300 {
                world.step(arrival_at(&arrivals, tick));
                if world.starts.contains_key(&6) {
                    break;
                }
            }
            let hung = world.running.iter().filter(|run| run.ends.is_none());
            most = most.max(hung.count());
            let started = world.starts.get(&6).copied();
            assert!(
                started.is_some_and(|started| started <= world.last_end + 1),
                "seed {seed}: the owner started at {started:?}, the last run not behind its barrier ended at {}",
                world.last_end
            );
        }
        assert!(most >= 3, "the caps fill with hung runners: {most}");
    }

    /// Minute ticks, so a job of one or two ticks is short (`SHORT_RUN`): a big job of more than half the slice, mid jobs
    /// of up to 20 minutes, and small ones.
    static SHORT_STREAMS: Shape = Shape {
        tick: Duration::from_secs(60),
        ..HEAVY_STREAMS
    };

    /// Two mid jobs and from the start a steady stream of short small jobs, five to seven each minute, each running
    /// three minutes, which the plan reads as two: enough to keep the big job's room taken for good if nothing held them
    /// back. The big job comes in the third minute, and a mid job now and then. Also the big job's ID.
    fn a_steady_stream_of_short_jobs(rng: &mut Rng, ticks: usize) -> (Vec<Vec<Job>>, u64) {
        let mut next = 1;
        let mut job = |pool, ticks| {
            let id = next;
            next += 1;
            Job { id, pool, ticks }
        };
        let mut arrivals = vec![Vec::new(); ticks];
        let mut big = 0;
        for (tick, now) in arrivals.iter_mut().enumerate() {
            if tick == 0 {
                now.extend([job(1, 5 + rng.below(15)), job(1, 5 + rng.below(15))]);
            }
            if tick == 2 {
                let owner = job(0, 3);
                big = owner.id;
                now.push(owner);
            }
            for _ in 0..5 + rng.below(3) {
                now.push(job(2, 3));
            }
            if rng.below(10) == 0 {
                now.push(job(1, 5 + rng.below(15)));
            }
        }
        (arrivals, big)
    }

    /// ADR 0151's promise against starvation: under a stream of short jobs that never lets up, with run times that read
    /// short so that only short jobs pass, and the daemon restarting now and then, the big job (job 6) starts, and no
    /// later than a tick after every runner that did not pass a barrier has ended. A restart forgets what passed, so
    /// those runners count among the ones it waits out.
    #[test]
    fn a_steady_stream_of_short_jobs_cannot_keep_a_barriers_owner_out() {
        const TICKS: usize = 240;
        let mut passed = 0;
        let mut restarted = 0;
        for seed in 0..64 {
            let mut rng = Rng(seed);
            let (arrivals, big) = a_steady_stream_of_short_jobs(&mut rng, TICKS);
            let mut world = World::new(&SHORT_STREAMS, Estimates::Half, Runs::OnTime);
            let mut started = None;
            let mut last_restart = 0;
            for tick in 0..TICKS as u64 {
                // Restarts while the owner waits, each past a barrier's first limit from the last: more often than the
                // limit would keep any barrier from standing (ADR 0096).
                if tick >= last_restart + 12 && rng.below(4) == 0 {
                    world.restart();
                    last_restart = tick;
                    restarted += 1;
                }
                world.step(arrival_at(&arrivals, tick));
                if let Some(at) = world.starts.get(&big) {
                    started = Some(*at);
                    break;
                }
            }
            passed += world.passed;
            assert!(
                started.is_some_and(|started| started <= world.last_end + 1),
                "seed {seed}: the owner started at {started:?}, the last run not behind its barrier ended at {}",
                world.last_end
            );
        }
        assert!(passed > 100, "short jobs pass: {passed}");
        assert!(
            restarted > 16,
            "and the daemon restarts while it waits: {restarted}"
        );
    }

    /// Every world of `count` jobs of a shape's three pools, running 1 to 4 ticks, each arriving with the one before or
    /// `gap` ticks after it as the bits of `gaps` say: world `index` of `worlds_of(count)`.
    fn world(count: u32, gaps: u32, gap: usize, index: u64) -> Vec<Vec<Job>> {
        let mut ticks: Vec<Vec<Job>> = vec![Vec::new()];
        let mut left = index;
        for id in 0..count {
            let pool = usize::try_from(left % 3).unwrap();
            left /= 3;
            let run = 1 + left % 4;
            left /= 4;
            if id > 0 && gaps >> (id - 1) & 1 == 1 {
                ticks.extend(std::iter::repeat_n(Vec::new(), gap));
            }
            ticks.last_mut().unwrap().push(Job {
                id: u64::from(id) + 1,
                pool,
                ticks: run,
            });
        }
        ticks
    }

    fn worlds_of(count: u32) -> u64 {
        12_u64.pow(count)
    }

    /// Checks both bounds and the ceiling on every world of up to `most` jobs, with every pattern of gaps below
    /// `gap_patterns(count)`, on `threads` threads.
    fn every_world(
        shape: &'static Shape,
        most: u32,
        gap_patterns: impl Fn(u32) -> u32 + Sync,
        threads: u64,
    ) -> Tally {
        let total = Mutex::new(Tally::default());
        std::thread::scope(|scope| {
            for thread in 0..threads {
                let (total, gap_patterns) = (&total, &gap_patterns);
                scope.spawn(move || {
                    let mut tally = Tally::default();
                    for count in 1..=most {
                        for gaps in 0..gap_patterns(count) {
                            let mut index = thread;
                            while index < worlds_of(count) {
                                let arrivals = world(count, gaps, shape.gap, index);
                                let name = || format!("{arrivals:?}");
                                tally.worlds += 1;
                                tally.compare(shape, Estimates::Exact, &arrivals, &name);
                                tally.compare(shape, Estimates::Double, &arrivals, &name);
                                tally.hang(shape, &arrivals);
                                index += threads;
                            }
                        }
                    }
                    total.lock().unwrap().add(tally);
                });
            }
        });
        total.into_inner().unwrap()
    }

    fn assert_bounds_hold(tally: &Tally) {
        let first = |found: &[String]| found.first().cloned();
        assert_eq!(first(&tally.late), None, "{} late", tally.late.len());
        assert_eq!(
            first(&tally.past_the_ceiling),
            None,
            "{} past the ceiling",
            tally.past_the_ceiling.len()
        );
        assert_eq!(
            first(&tally.kept_out),
            None,
            "{} kept out",
            tally.kept_out.len()
        );
    }

    /// `shape` with `credit`, for as long as the test runs.
    fn pooled(shape: &'static Shape, credit: Credit) -> &'static Shape {
        Box::leak(Box::new(Shape {
            credit: Some(credit),
            ..*shape
        }))
    }

    /// The bounds with a credit that comes at every moment of a small world and leaves soon after or stays: nothing that
    /// passes stands in the way of room that comes sooner than ends bring it, and the owner still fits once the rest
    /// ended, the credit gone or not.
    #[test]
    fn in_every_small_world_a_credit_that_comes_and_goes_breaks_no_promise_and_a_barrier_still_ends()
     {
        let mut passed = 0;
        for shape in [&TIGHT, &SHARED] {
            for from in 1..4 {
                for until in [from + 2, u64::MAX] {
                    let most = 3 * GIB;
                    let credit = Credit { from, until, most };
                    let tally = every_world(pooled(shape, credit), 3, |count| 1 << (count - 1), 4);
                    assert_bounds_hold(&tally);
                    passed += tally.passed;
                }
            }
        }
        assert!(passed > 400, "jobs pass in these worlds: {passed}");
    }

    /// The bounds, on every small world rather than on a sample of large ones.
    #[test]
    fn in_every_small_world_an_owner_keeps_what_adr_0097_promises() {
        let tally = every_world(&TIGHT, 3, |count| 1 << (count - 1), 1);
        assert_bounds_hold(&tally);
        assert!(tally.passed > 100, "jobs pass in these worlds: {tally:?}");
        assert!(tally.hung > 100, "and hang: {tally:?}");
        let tally = every_world(&TWO_LIMITS, 4, |count| 1 << (count - 1), 1);
        assert_bounds_hold(&tally);
        assert!(tally.passed > 100, "and with two limits: {tally:?}");
    }

    /// The case the first bound excuses, as no run of fewer jobs reaches it: job 5 owns a barrier, older jobs take it, and
    /// on estimates that read double what passed starts it a tick late. Found by `every_world_of_six`.
    #[test]
    fn an_owner_that_loses_its_barrier_can_start_later_than_under_a_strict_one() {
        let job = |id, pool, ticks| Job { id, pool, ticks };
        let arrivals = [
            vec![job(1, 2, 4), job(2, 1, 2), job(3, 1, 3)],
            vec![job(4, 1, 1), job(5, 0, 1), job(6, 2, 2)],
        ];
        let mut tally = Tally::default();
        tally.compare(&TIGHT, Estimates::Double, &arrivals, &|| {
            "pinned".to_owned()
        });
        assert!(tally.lost_and_late > 0, "{tally:?}");
        assert_eq!(tally.past_the_ceiling, [] as [String; 0]);
    }

    /// The same up to 6 jobs: minutes in a release build, so not on every run. `cargo test --release -- --ignored
    /// every_world_of_six`. Rule 2's older jobs change no count in these worlds: their unit tests and the replay and
    /// planner's shared fixture stand for them.
    #[test]
    #[ignore = "takes minutes; ADRs 0097, 0098 and 0131 record its last result"]
    #[allow(
        clippy::print_stderr,
        reason = "its counts are what ADRs 0097, 0098 and 0131 record"
    )]
    fn every_world_of_six() {
        for (shape, most, lost) in [
            (&SHARED, 5, false),
            (&TIGHT, 6, true),
            // No owner that lost its barrier starts late here once runners started for older jobs stopped closing
            // backfill (ADR 0131): 80 did before.
            (&TWO_LIMITS, 6, false),
        ] {
            let tally = every_world(shape, most, |count| 1 << (count - 1).min(3), 8);
            eprintln!("{tally:?}");
            assert_bounds_hold(&tally);
            assert!(tally.hung > 100, "jobs hang in these worlds: {tally:?}");
            assert_eq!(tally.lost_and_late > 0, lost, "{tally:?}");
        }
    }
}
