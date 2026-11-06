//! What starts behind a barrier (ADR 0097), as `plan` decides it: the rule's four parts, each failing on its own, and
//! over generated sequences its two bounds, with run times that hold and with run times that do not.

use super::*;
use crate::scheduler::{Drained, MEMORY_PRIORITY_AFTER};

const MINUTE: Duration = Duration::from_secs(60);
/// When the owner's wait began.
const WAITED: Duration = Duration::from_secs(600);
/// Started before the owner's barrier matured.
const BEFORE: Duration = Duration::from_mins(15);

fn minutes(minutes: u32) -> Duration {
    MINUTE * minutes
}

fn starved(job: u64) -> Waits {
    Waits {
        memory: Some(Wait {
            job,
            since: now() - WAITED,
        }),
        ..Waits::default()
    }
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

#[test]
fn a_pool_with_no_estimate_stays_behind_the_barrier() {
    assert_held(&plan_behind(younger(None), room_in_twenty_minutes()));
}

/// An overdue runner may end any moment, so no time is safe to fill.
#[test]
fn nothing_passes_while_a_runner_that_gives_the_room_is_past_its_usual_end() {
    let overdue = Slot::Runner(RunnerSlot {
        repository: "o/other".to_owned(),
        busy: true,
        started_at: now() - BEFORE,
        memory: 4 * GIB,
        cpu: 0,
        usual: Some(BEFORE.saturating_sub(MINUTE)),
    });
    let runners = || vec![overdue.clone(), running(8 * GIB, minutes(40))];
    assert_held(&plan_behind(younger(Some(minutes(1))), runners()));
    // Not even a job that takes no time.
    assert_held(&plan_behind(younger(Some(Duration::ZERO)), runners()));
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
        })
    };
    for runner in [unknown(true, None), unknown(false, Some(minutes(60)))] {
        let runners = vec![runner, running(8 * GIB, minutes(40))];
        assert_held(&plan_behind(younger(Some(minutes(1))), runners));
    }
}

/// Room held by something no runner's end gives back: the 4G that ends leaves the owner short, so no time is known.
#[test]
fn nothing_passes_while_no_runners_end_brings_the_room() {
    let plan_with = |left| {
        plan(
            &[owner(), younger(Some(minutes(1)))],
            &slots(vec![running(4 * GIB, minutes(20))]),
            Budget {
                memory: Some(left),
                ..slice(0)
            },
            now(),
        )
    };
    assert_passes(&plan_with(gib(4)));
    assert_held(&plan_with(gib(3)));
}

fn gib(amount: i64) -> i64 {
    amount * i64::try_from(GIB).unwrap()
}

/// What started since the barrier matured may hang: at their caps they must leave the owner its room.
#[test]
fn the_caps_of_what_started_since_the_barrier_matured_leave_the_owner_its_room() {
    let since = now() - WAITED + MEMORY_PRIORITY_AFTER;
    let backfilled = |started_at| {
        Slot::Runner(RunnerSlot {
            repository: "o/other".to_owned(),
            busy: true,
            started_at,
            memory: GIB,
            cpu: 0,
            usual: Some(minutes(600)),
        })
    };
    // 8G caps in `other`: with the owner's 8G they take all a drain gives.
    let runners = |started_at| vec![running(10 * GIB, minutes(20)), backfilled(started_at)];
    assert_held(&plan_behind(younger(Some(minutes(1))), runners(since)));
    assert_passes(&plan_behind(
        younger(Some(minutes(1))),
        runners(since - Duration::from_secs(1)),
    ));
}

#[test]
fn the_caps_of_a_plans_own_starts_count_too() {
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
    assert_held(&plan_with(gib(-1), minutes(21)));
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
    let starved_of_cpu = Waits {
        cpu: Some(Wait {
            job: 1,
            since: now() - WAITED,
        }),
        ..Waits::default()
    };
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
            budget,
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
        &[older, owner(), younger(Some(minutes(1)))],
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
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
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
        })
    };
    let runners = || vec![from_the_future(minutes(5)), running(8 * GIB, minutes(40))];
    let held = plan_behind(younger(Some(minutes(6))), runners());
    assert_held(&held);
    // Started "later" than the barrier matured, so its cap counts as well: 8G, all a drain leaves past the owner.
    let wait_from_the_future = Repository {
        waits: Waits {
            memory: Some(Wait {
                job: 1,
                since: now() + minutes(1),
            }),
            ..Waits::default()
        },
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

/// The planner, a model of the engine and of GitHub: jobs queue, runners take the oldest job of their pool and run as
/// long as a run time says, and nothing but the plan decides. Run times here are the model's own.
mod generated {
    use super::*;

    const TICK: Duration = Duration::from_secs(30);
    const BUDGET: u64 = 16 * GIB;
    const HUNG_JOBS_GROW_AT: u64 = 30;

    /// Each pool's repository, profile, what a runner takes and what its cap lets it grow to.
    const POOLS: [(&str, &str, u64, u64); 3] = [
        ("o/big", "big", 8 * GIB, 8 * GIB),
        ("o/mid", "mid", 3 * GIB, 3 * GIB),
        ("o/small", "small", GIB, 2 * GIB),
    ];

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
        /// How many ticks it usually runs.
        usual: u64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Estimates {
        /// The planner knows none: a strict barrier.
        None,
        /// The planner knows each job's.
        Known,
    }

    /// What a run does with its usual run time.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Runs {
        /// It holds.
        AsUsual,
        /// A job that started behind a barrier never ends; the rest hold.
        BackfilledHang,
    }

    #[derive(Debug, Clone)]
    struct Run {
        job: Job,
        started: u64,
        /// `None`: it hangs.
        ends: Option<u64>,
        behind: bool,
    }

    fn at(tick: u64) -> SystemTime {
        now() + TICK * u32::try_from(tick).unwrap()
    }

    impl Run {
        /// What it takes of the budget at `tick`: a job that hangs grows to its cap, late enough that the budgets gave
        /// no warning while jobs passed, which only rule 4 of ADR 0097 stands against.
        fn takes(&self, tick: u64) -> u64 {
            let (.., memory, cap) = POOLS[self.job.pool];
            if self.ends.is_none() && tick >= HUNG_JOBS_GROW_AT {
                cap
            } else {
                memory
            }
        }
    }

    struct World {
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
    }

    impl World {
        fn new(estimates: Estimates, runs: Runs) -> World {
            World {
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
            }
        }

        /// The same world from here on, knowing `estimates`.
        fn fork(&self, estimates: Estimates) -> World {
            World {
                estimates,
                queued: self.queued.clone(),
                running: self.running.clone(),
                starts: self.starts.clone(),
                owners: self.owners.clone(),
                ..*self
            }
        }

        /// Whether nothing is left to arrive, wait or run.
        fn done(&self, arrivals: &[Vec<Job>]) -> bool {
            usize::try_from(self.tick).unwrap() >= arrivals.len()
                && self.queued.is_empty()
                && self.running.is_empty()
        }

        fn repositories(&self) -> Vec<Repository> {
            (0..POOLS.len())
                .map(|index| {
                    let (name, profile, memory, _) = POOLS[index];
                    let jobs: Vec<&Job> =
                        self.queued.iter().filter(|job| job.pool == index).collect();
                    let longest = jobs.iter().map(|job| job.usual).max();
                    let ids: Vec<u64> = jobs.iter().map(|job| job.id).collect();
                    Repository {
                        waits: self.waits[index],
                        ..pool(
                            name,
                            profile,
                            memory,
                            longest
                                .filter(|_| self.estimates == Estimates::Known)
                                .map(|ticks| TICK * u32::try_from(ticks).unwrap()),
                            &ids,
                        )
                    }
                })
                .collect()
        }

        fn profiles(&self) -> BTreeMap<String, Profile> {
            let mut profiles = BTreeMap::new();
            for (index, (_, name, _, cap)) in POOLS.iter().enumerate() {
                let mut slots: Vec<Slot> = self
                    .running
                    .iter()
                    .filter(|run| run.job.pool == index)
                    .map(|run| {
                        Slot::Runner(RunnerSlot {
                            repository: POOLS[index].0.to_owned(),
                            busy: true,
                            started_at: at(run.started),
                            memory: run.takes(self.tick),
                            cpu: 0,
                            usual: (self.estimates == Estimates::Known)
                                .then(|| TICK * u32::try_from(run.job.usual).unwrap()),
                        })
                    })
                    .collect();
                slots.resize(slots.len().max(16), Slot::Free);
                profiles.insert((*name).to_owned(), profile(*cap, 0, slots));
            }
            profiles
        }

        /// One tick: what is due ends, `arrivals` queue, and one plan is carried out as the engine would.
        fn step(&mut self, arrivals: &[Job]) -> Plan {
            let tick = self.tick;
            self.running
                .retain(|run| run.ends.is_none_or(|ends| ends > tick));
            self.queued.extend_from_slice(arrivals);
            let used: u64 = self.running.iter().map(|run| run.takes(tick)).sum();
            let budget = Budget {
                memory: Some(crate::scheduler::left(BUDGET, used)),
                drained: Drained {
                    memory: Some(crate::scheduler::left(BUDGET, 0)),
                    ..Drained::default()
                },
                ..Budget::default()
            };
            let at = at(tick);
            let plan = plan(&self.repositories(), &self.profiles(), budget, at);
            for decision in &plan.decisions {
                let Decision::Start { repository, .. } = decision else {
                    continue;
                };
                let index = POOLS
                    .iter()
                    .position(|(name, ..)| name == repository)
                    .unwrap();
                let oldest = self
                    .queued
                    .iter()
                    .position(|job| job.pool == index)
                    .unwrap();
                let job = self.queued.remove(oldest);
                let behind = plan.barrier.is_some_and(|barrier| barrier.job < job.id);
                self.passed += u64::from(behind);
                self.waits[index] = Waits::default();
                self.starts.insert(job.id, tick);
                let ends = (!behind || self.runs == Runs::AsUsual).then_some(tick + job.usual);
                if !behind {
                    self.last_end = self.last_end.max(tick + job.usual);
                }
                self.running.push(Run {
                    job,
                    started: tick,
                    ends,
                    behind,
                });
            }
            for demand in &plan.held {
                let index = POOLS
                    .iter()
                    .position(|(name, ..)| *name == demand.pool.repository)
                    .unwrap();
                self.waits[index].note(demand.scarce, demand.job, at);
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
        let mut job = |pool, usual| {
            let id = next;
            next += 1;
            Job { id, pool, usual }
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

    /// The first bound of ADR 0097, at every barrier of a run: from the plan that first put it up, the same world goes on
    /// with younger jobs passing and with none, and its owner starts no later for them.
    #[test]
    fn with_run_times_that_hold_every_owner_starts_as_early_as_under_a_strict_barrier() {
        let (mut owners, mut passed, mut later) = (0, 0, Vec::new());
        for seed in 0..128 {
            let arrivals = arrivals(&mut Rng(seed));
            let mut world = World::new(Estimates::Known, Runs::AsUsual);
            let mut seen = 0;
            while !world.done(&arrivals) {
                world.step(arrival_at(&arrivals, world.tick));
                for owner in world.owners[seen..].iter().copied() {
                    owners += 1;
                    let start = |estimates| {
                        let mut fork = world.fork(estimates);
                        while !fork.starts.contains_key(&owner) {
                            fork.step(arrival_at(&arrivals, fork.tick));
                        }
                        fork.starts[&owner]
                    };
                    let (strict, backfill) = (start(Estimates::None), start(Estimates::Known));
                    if backfill > strict {
                        later.push((seed, owner, world.tick, strict, backfill));
                    }
                }
                seen = world.owners.len();
            }
            passed += world.passed;
        }
        assert_eq!(later, [], "seed, owner, tick, strict start, backfill start");
        assert!(owners > 100, "generated runs reach barriers: {owners}");
        assert!(passed > 20, "and younger jobs pass them: {passed}");
    }

    /// Five mid runners filling the slice and ending one by one, a big job behind them, and a stream of short small jobs.
    fn a_stream_behind_a_big_job(rng: &mut Rng) -> Vec<Vec<Job>> {
        let mut ticks = vec![Vec::new(); 80];
        for id in 1..=5 {
            let usual = 10 * id + rng.below(10);
            ticks[0].push(Job { id, pool: 1, usual });
        }
        ticks[1].push(Job {
            id: 6,
            pool: 0,
            usual: 5,
        });
        for (tick, id) in (2..80).zip(7..) {
            let usual = 1 + rng.below(3);
            ticks[tick].push(Job { id, pool: 2, usual });
        }
        ticks
    }

    /// The second bound of ADR 0097: whatever started behind the barrier does, the owner fits once the rest ended.
    #[test]
    fn jobs_behind_the_barrier_that_never_end_cannot_keep_the_owner_out() {
        let mut most = 0;
        for seed in 0..64 {
            let arrivals = a_stream_behind_a_big_job(&mut Rng(seed));
            let mut world = World::new(Estimates::Known, Runs::BackfilledHang);
            for tick in 0..300 {
                world.step(arrival_at(&arrivals, tick));
                if world.starts.contains_key(&6) {
                    break;
                }
            }
            most = most.max(world.running.iter().filter(|run| run.behind).count());
            let started = world.starts.get(&6).copied();
            assert!(
                started.is_some_and(|started| started <= world.last_end + 1),
                "seed {seed}: the owner started at {started:?}, the last run not behind its barrier ended at {}",
                world.last_end
            );
        }
        assert!(most >= 3, "the caps fill with hung runners: {most}");
    }
}
