//! The barrier for a starved job (ADR 0096) as `plan` puts it up: who owns it, who waits behind it and why, what never
//! erects one, and over generated sequences, that it neither stalls the queue nor outlives its job.

use super::*;
use crate::scheduler::{Barrier, Drained, MEMORY_PRIORITY_AFTER};

const LONG_AGO: Duration = Duration::from_secs(600);

fn waited(job: u64) -> Wait {
    Wait {
        job,
        since: now() - LONG_AGO,
    }
}

/// A pool of `name` in `app` with runners of `memory`, whose next job `waits`.
fn pool(name: &str, memory: u64, waits: Waits, queued: &[u64]) -> Repository {
    Repository {
        runner_memory: Some(memory),
        waits,
        ..repository(
            name,
            "app",
            queued.iter().map(|id| job(*id, &["app"])).collect(),
        )
    }
}

fn starved_of_memory(job: u64) -> Waits {
    Waits {
        memory: Some(waited(job)),
        ..Waits::default()
    }
}

/// `free` of the memory budget left, an empty slice having room for 16G.
fn memory_left(free: i64) -> Budget {
    Budget {
        memory: Some(free),
        drained: Drained {
            memory: Some(16 * i64::try_from(GIB).unwrap()),
            ..Drained::default()
        },
        ..Budget::default()
    }
}

fn gib(amount: i64) -> i64 {
    amount * i64::try_from(GIB).unwrap()
}

fn a_big_job_and_a_small_one(waits: Waits) -> [Repository; 2] {
    [
        pool("o/big", 8 * GIB, waits, &[1]),
        pool("o/small", GIB, Waits::default(), &[2]),
    ]
}

/// The bug: a memory shortage only `continue`d, so a stream of small runners took all that freed up.
#[test]
fn younger_jobs_wait_behind_one_starved_of_memory_though_they_fit() {
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(starts(&plan.decisions), []);
    assert_eq!(
        plan.waiting,
        BTreeMap::from([
            (
                1,
                Waiting::Memory {
                    needs: 8 * GIB,
                    free: 4 * GIB
                }
            ),
            (2, Waiting::BehindMemory { job: 1 }),
        ])
    );
    assert_eq!(
        plan.barrier,
        Some(Barrier {
            job: 1,
            scarce: Scarce::Memory
        })
    );
}

/// A barrier from the first shortage would idle the slice for every job that waits out one short run.
#[test]
fn a_job_short_of_memory_lets_younger_ones_past_until_it_waited_long_enough() {
    let just_now = Waits {
        memory: Some(Wait {
            job: 1,
            since: now() - MEMORY_PRIORITY_AFTER + Duration::from_secs(1),
        }),
        ..Waits::default()
    };
    let plan = plan(
        &a_big_job_and_a_small_one(just_now),
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(starts(&plan.decisions), [(0, "o/small")]);
    assert_eq!(plan.barrier, None);
    assert!(
        plan.held.contains(&held("o/big", 1, Scarce::Memory)),
        "the plan names the job whose wait goes on"
    );
}

/// A wait is its job's: one left over from a job gone must not make the next job of the pool jump the queue.
#[test]
fn a_wait_counts_for_its_own_job_alone() {
    let plan = plan(
        &[
            pool("o/big", 8 * GIB, starved_of_memory(9), &[1]),
            pool("o/small", GIB, Waits::default(), &[2]),
        ],
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(starts(&plan.decisions), [(0, "o/small")]);
}

/// `Budget::memory` is what is left, not the budget: reading it as the total, or its deficit as 0, would either drop
/// exactly the barriers wanted here or invent room.
#[test]
fn a_budget_overdrawn_now_still_erects_a_barrier_as_draining_repays_it() {
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(-3)),
        now(),
    );
    assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(1));
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

/// A hold no drain can end would stop every younger job for good.
#[test]
fn a_runner_no_empty_slice_holds_erects_no_barrier() {
    let budget = Budget {
        drained: Drained {
            memory: Some(gib(7)),
            ..Drained::default()
        },
        ..memory_left(gib(4))
    };
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        budget,
        now(),
    );
    assert_eq!(plan.barrier, None);
    assert_eq!(starts(&plan.decisions), [(0, "o/small")]);
    assert_eq!(
        plan.waiting[&1],
        Waiting::Memory {
            needs: 8 * GIB,
            free: 4 * GIB
        },
        "it says why it waits as before"
    );
}

/// Programs outside the slice hold the host: draining CI gives that room to nobody, so younger jobs go on.
#[test]
fn a_host_short_even_with_an_empty_slice_erects_no_barrier() {
    let budget = Budget {
        memory: Some(gib(12)),
        host: Some(gib(4)),
        drained: Drained {
            memory: Some(gib(16)),
            host: Some(gib(6)),
            ..Drained::default()
        },
        ..Budget::default()
    };
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        budget,
        now(),
    );
    assert_eq!(plan.barrier, None);
    assert_eq!(starts(&plan.decisions), [(0, "o/small")]);
    assert!(
        matches!(plan.waiting[&1], Waiting::HostMemory { .. }),
        "{:?}",
        plan.waiting
    );
}

/// With no budget and no host figure nothing is short: a stale wait must change nothing.
#[test]
fn without_a_memory_budget_or_a_host_figure_a_wait_changes_nothing() {
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        Budget::default(),
        now(),
    );
    assert_eq!(starts(&plan.decisions), [(0, "o/big"), (1, "o/small")]);
    assert_eq!(plan.barrier, None);
}

/// Host-only admission has no slice budget, and must still protect a starved job.
#[test]
fn a_host_figure_alone_erects_a_barrier() {
    let budget = Budget {
        host: Some(gib(4)),
        drained: Drained {
            host: Some(gib(16)),
            ..Drained::default()
        },
        ..Budget::default()
    };
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        budget,
        now(),
    );
    assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(1));
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

/// The interlocks' zeros say nothing of the room there is: no job's wait starts from them and none holds on them.
#[test]
fn a_safety_interlock_suspends_the_barrier_and_starts_no_wait() {
    let budget = Budget {
        held: true,
        ..memory_left(0)
    };
    let plan = plan(
        &[
            pool("o/big", 8 * GIB, starved_of_memory(1), &[1]),
            pool("o/small", GIB, Waits::default(), &[2]),
        ],
        &profiles(vec![Slot::Free, Slot::Free]),
        budget,
        now(),
    );
    assert_eq!(plan.barrier, None);
    assert_eq!(starts(&plan.decisions), []);
    assert_eq!(
        plan.waiting[&2],
        Waiting::Memory {
            needs: GIB,
            free: 0
        }
    );
    assert_eq!(plan.held, []);
}

/// Two barriers would let two large jobs block each other's room; the second waits, and keeps ageing to go next.
#[test]
fn a_second_starved_pool_waits_behind_the_first_and_keeps_ageing() {
    let plan = plan(
        &[
            pool("o/first", 8 * GIB, starved_of_memory(1), &[1]),
            pool("o/second", 8 * GIB, Waits::default(), &[2]),
            pool("o/small", GIB, Waits::default(), &[3]),
        ],
        &profiles(vec![Slot::Free; 3]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(1));
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
    assert_eq!(plan.waiting[&3], Waiting::BehindMemory { job: 1 });
    // A job that fits lacks nothing but its turn.
    assert_eq!(
        plan.held,
        [
            held("o/first", 1, Scarce::Memory),
            held("o/second", 2, Scarce::Memory)
        ]
    );
}

/// An older job keeps its place: a barrier only binds those behind it.
#[test]
fn an_older_job_that_fits_starts_before_the_barrier_of_a_younger_one() {
    let plan = plan(
        &[
            pool("o/small", GIB, Waits::default(), &[1]),
            pool("o/big", 8 * GIB, starved_of_memory(2), &[2]),
        ],
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(starts(&plan.decisions), [(0, "o/small")]);
    assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(2));
}

/// The barrier falls the moment its job's runner fits: younger ones that fit beside it start in the same plan.
#[test]
fn the_owner_starting_in_this_plan_erects_no_barrier() {
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(9)),
        now(),
    );
    assert_eq!(starts(&plan.decisions), [(0, "o/big"), (1, "o/small")]);
    assert_eq!(plan.barrier, None);
}

/// A job with no free slot could not use the memory a barrier saves; holding others for it would idle the slice.
#[test]
fn a_starved_pool_without_a_free_slot_erects_no_barrier_and_keeps_its_wait() {
    let repositories = [
        pool("o/big", 8 * GIB, starved_of_memory(1), &[1]),
        Repository {
            runner_memory: Some(GIB),
            ..repository("o/small", "small", vec![job(2, &["small"])])
        },
    ];
    let mut profiles = profiles(vec![Slot::Runner(RunnerSlot {
        repository: "o/other".to_owned(),
        busy: true,
        started_at: now(),
        memory: 4 * GIB,
        cpu: 2000,
    })]);
    profiles.insert(
        "small".to_owned(),
        Profile {
            memory: GIB,
            cpus: 1000,
            preparing: None,
            upkeep_due: false,
            count: 1,
            ran: Vec::new(),
            slots: vec![Slot::Free],
        },
    );
    let plan = plan(&repositories, &profiles, memory_left(gib(4)), now());
    assert_eq!(plan.barrier, None);
    assert_eq!(starts(&plan.decisions), [(0, "o/small")]);
    assert_eq!(
        plan.waiting[&1],
        Waiting::NoSlot {
            profile: "app".to_owned()
        }
    );
}

/// A wait for one resource must count when the shortage moves to the other, or the job goes back to the end each time.
#[test]
fn a_job_starved_of_cpu_that_now_lacks_memory_owns_the_barrier_as_memory() {
    let waits = Waits {
        cpu: Some(waited(1)),
        ..Waits::default()
    };
    let plan = plan(
        &a_big_job_and_a_small_one(waits),
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(
        plan.barrier,
        Some(Barrier {
            job: 1,
            scarce: Scarce::Memory
        })
    );
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

#[test]
fn a_job_starved_of_memory_that_now_lacks_cpu_owns_the_barrier_as_cpu() {
    let repositories = [
        Repository {
            runner_cpu: Some(4000),
            ..pool("o/big", 8 * GIB, starved_of_memory(1), &[1])
        },
        Repository {
            runner_cpu: Some(500),
            ..pool("o/small", GIB, Waits::default(), &[2])
        },
    ];
    let budget = Budget {
        cpu: Some(1000),
        ..memory_left(gib(12))
    };
    let plan = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        budget,
        now(),
    );
    assert_eq!(
        plan.barrier,
        Some(Barrier {
            job: 1,
            scarce: Scarce::Cpu
        })
    );
    assert_eq!(plan.waiting[&2], Waiting::BehindCpu { job: 1 });
}

/// Each pool protecting the resource the other needs would be a deadlock: the older demand alone owns the plan.
#[test]
fn opposing_demands_share_one_barrier_the_older_ones() {
    let repositories = [
        Repository {
            runner_cpu: Some(500),
            ..pool("o/memory", 8 * GIB, starved_of_memory(1), &[1])
        },
        Repository {
            runner_cpu: Some(4000),
            ..pool(
                "o/cpu",
                GIB,
                Waits {
                    cpu: Some(waited(2)),
                    ..Waits::default()
                },
                &[2],
            )
        },
    ];
    let budget = Budget {
        cpu: Some(1000),
        ..memory_left(gib(4))
    };
    let plan = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        budget,
        now(),
    );
    assert_eq!(plan.barrier.map(|barrier| barrier.job), Some(1));
    assert_eq!(plan.waiting[&2], Waiting::BehindMemory { job: 1 });
}

/// A paused pool, or one backing off, cannot use what a barrier saves.
#[test]
fn a_starved_pool_that_may_not_start_erects_no_barrier() {
    for (paused, held) in [
        (true, None),
        (
            false,
            Some(Hold {
                until: now() + LONG_AGO,
                cause: HoldCause::Start,
            }),
        ),
    ] {
        let mut repositories = a_big_job_and_a_small_one(starved_of_memory(1));
        repositories[0].paused = paused;
        repositories[0].held = held;
        let plan = plan(
            &repositories,
            &profiles(vec![Slot::Free, Slot::Free]),
            memory_left(gib(4)),
            now(),
        );
        assert_eq!(plan.barrier, None);
        assert_eq!(starts(&plan.decisions), [(0, "o/small")]);
    }
}

/// Behind a barrier a job keeps the reason that is true of it: reseeding, or no slot, is not what memory priority
/// does, and another slot would not start it, so auto sizing must not read it as a wait for a slot.
#[test]
fn a_job_behind_a_barrier_with_no_slot_says_so_and_asks_for_no_slot() {
    let repositories = [
        pool("o/big", 8 * GIB, starved_of_memory(1), &[1]),
        Repository {
            runner_memory: Some(GIB),
            ..repository("o/full", "full", vec![job(2, &["full"])])
        },
        Repository {
            runner_memory: Some(GIB),
            ..repository("o/seeding", "seeding", vec![job(3, &["seeding"])])
        },
    ];
    let mut profiles = profiles(vec![Slot::Free]);
    let other = Profile {
        memory: GIB,
        cpus: 1000,
        preparing: None,
        upkeep_due: false,
        count: 1,
        ran: Vec::new(),
        slots: vec![Slot::Runner(RunnerSlot {
            repository: "o/other".to_owned(),
            busy: true,
            started_at: now(),
            memory: GIB,
            cpu: 1000,
        })],
    };
    profiles.insert("full".to_owned(), other.clone());
    profiles.insert(
        "seeding".to_owned(),
        Profile {
            preparing: Some(Preparing::Reseeding),
            ..other
        },
    );
    let plan = plan(&repositories, &profiles, memory_left(gib(4)), now());
    assert_eq!(
        plan.waiting[&2],
        Waiting::NoSlot {
            profile: "full".to_owned()
        }
    );
    assert_eq!(
        plan.waiting[&3],
        Waiting::Reseeding {
            profile: "seeding".to_owned()
        }
    );
    assert_eq!(plan.slot_waiting, BTreeSet::new());
}

/// A job behind a barrier that fits but for it would otherwise read as one more slot wanted.
#[test]
fn a_job_behind_a_barrier_is_no_slot_wait() {
    let plan = plan(
        &a_big_job_and_a_small_one(starved_of_memory(1)),
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(plan.slot_waiting, BTreeSet::new());
}

/// A warm runner starting into the room would take what the barrier saves.
#[test]
fn no_warm_runner_starts_while_a_barrier_stands() {
    let repositories = [
        pool("o/big", 8 * GIB, starved_of_memory(1), &[1]),
        Repository {
            warm: 1,
            ..pool("o/warm", GIB, Waits::default(), &[])
        },
    ];
    let plan = plan(
        &repositories,
        &profiles(vec![Slot::Free, Slot::Free]),
        memory_left(gib(5)),
        now(),
    );
    assert_eq!(starts(&plan.decisions), []);
}

fn warm_runner(repository: &str, memory: u64) -> Slot {
    Slot::Runner(RunnerSlot {
        repository: repository.to_owned(),
        busy: false,
        started_at: now(),
        memory,
        cpu: 500,
    })
}

/// Warm runners stop only for a start they make possible: stopping some for room that still would not do loses them
/// for nothing, and a younger job behind the barrier may not reclaim them at all.
#[test]
fn warm_runners_stop_only_for_the_owners_whole_start() {
    let warm = |waiting: &[u64], free: i64| {
        let repositories = [
            pool("o/big", 8 * GIB, starved_of_memory(1), &[1]),
            Repository {
                warm: 1,
                ..pool("o/warm", GIB, Waits::default(), waiting)
            },
        ];
        plan(
            &repositories,
            &profiles(vec![Slot::Free, warm_runner("o/warm", GIB), Slot::Free]),
            memory_left(free),
            now(),
        )
    };
    let short = warm(&[], gib(4));
    assert_eq!(short.barrier.map(|barrier| barrier.job), Some(1));
    assert!(
        !short
            .decisions
            .iter()
            .any(|decision| matches!(decision, Decision::Stop { .. })),
        "{:?}",
        short.decisions
    );
    let enough = warm(&[], gib(7));
    assert_eq!(enough.barrier, None);
    assert_eq!(starts(&enough.decisions), [(0, "o/big")]);
    assert!(
        enough.decisions.contains(&Decision::Stop {
            slot: SlotRef {
                profile: "app".to_owned(),
                index: 1
            },
            reason: StopReason::Room
        }),
        "{:?}",
        enough.decisions
    );
}

/// Unknown labels and an empty queue have no demand to own anything.
#[test]
fn a_job_no_pool_serves_erects_no_barrier() {
    let plan = plan(
        &[pool("o/big", 8 * GIB, starved_of_memory(1), &[])],
        &profiles(vec![Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(plan, Plan::default());
    let unserved = [Repository {
        queued: vec![job(1, &["gpu"])],
        ..pool("o/big", 8 * GIB, starved_of_memory(1), &[])
    }];
    let plan = super::plan(
        &unserved,
        &profiles(vec![Slot::Free]),
        memory_left(gib(4)),
        now(),
    );
    assert_eq!(plan.barrier, None);
    assert!(
        matches!(plan.waiting[&1], Waiting::NoPool { .. }),
        "{:?}",
        plan.waiting
    );
}

/// The planner and a model of the engine over generated sequences: jobs queue, runners finish, listings fail, the
/// daemon restarts, a reload changes what a runner takes or removes the pool. Durations are bounded, as the ADR's
/// guarantee assumes.
mod generated {
    use super::*;

    const TICK: Duration = Duration::from_secs(30);
    /// The longest a job here runs, in ticks.
    const LONGEST: u64 = 12;
    const BUDGET: u64 = 16 * GIB;
    const POOLS: [&str; 3] = ["o/big", "o/mid", "o/small"];

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

    struct Running {
        memory: u64,
        ends: u64,
    }

    struct Model {
        tick: u64,
        next_job: u64,
        queued: [Vec<u64>; 3],
        waits: [Waits; 3],
        memory: [u64; 3],
        removed: [bool; 3],
        running: Vec<Running>,
    }

    impl Model {
        fn new() -> Model {
            Model {
                tick: 0,
                next_job: 1,
                queued: [Vec::new(), Vec::new(), Vec::new()],
                waits: [Waits::default(); 3],
                memory: [12 * GIB, 6 * GIB, GIB],
                removed: [false; 3],
                running: Vec::new(),
            }
        }

        fn at(&self) -> SystemTime {
            now() + TICK * u32::try_from(self.tick).unwrap()
        }

        fn queue(&mut self, pool: usize) {
            self.queued[pool].push(self.next_job);
            self.next_job += 1;
        }

        /// One plan from listings that succeeded, or failed for `unlisted`; the model commits it as `note_waits` and
        /// `apply_start` do, every start going through. Returns the plan.
        fn plan(&mut self, unlisted: Option<usize>, rng: &mut Rng) -> Plan {
            let at = self.at();
            let in_use: u64 = self.running.iter().map(|running| running.memory).sum();
            let repositories: Vec<Repository> = (0..POOLS.len())
                .filter(|pool| !self.removed[*pool])
                .map(|pool| {
                    let queued = if unlisted == Some(pool) {
                        &[][..]
                    } else {
                        &self.queued[pool][..]
                    };
                    super::pool(POOLS[pool], self.memory[pool], self.waits[pool], queued)
                })
                .collect();
            let slots = vec![Slot::Free; 8usize.saturating_sub(self.running.len())];
            let budget = Budget {
                memory: Some(crate::scheduler::left(BUDGET, in_use)),
                drained: Drained {
                    memory: Some(crate::scheduler::left(BUDGET, 0)),
                    ..Drained::default()
                },
                ..Budget::default()
            };
            let plan = plan(&repositories, &profiles(slots), budget, at);
            for decision in &plan.decisions {
                if let Decision::Start {
                    repository, memory, ..
                } = decision
                {
                    let pool = POOLS.iter().position(|name| name == repository).unwrap();
                    // GitHub gives the runner any of its pool's jobs.
                    let taken = usize::try_from(rng.below(self.queued[pool].len() as u64));
                    self.queued[pool].remove(taken.unwrap());
                    self.waits[pool] = Waits::default();
                    let ends = self.tick + 1 + rng.below(LONGEST);
                    self.running.push(Running {
                        memory: *memory,
                        ends,
                    });
                }
            }
            for demand in &plan.held {
                let pool = POOLS
                    .iter()
                    .position(|name| *name == demand.pool.repository)
                    .unwrap();
                self.waits[pool].note(demand.scarce, demand.job, at);
            }
            for pool in 0..POOLS.len() {
                if unlisted != Some(pool) {
                    let queued = &self.queued[pool];
                    self.waits[pool].retain(|job| queued.contains(&job));
                }
            }
            plan
        }

        /// Time passes, and the running jobs due end.
        fn elapse(&mut self) {
            self.tick += 1;
            let tick = self.tick;
            self.running.retain(|running| running.ends > tick);
        }
    }

    /// What a plan must hold, whatever came before it.
    fn holds(model: &Model, plan: &Plan, in_use_before: u64) {
        let behind: BTreeSet<u64> = plan
            .waiting
            .values()
            .filter_map(|reason| match reason {
                Waiting::BehindMemory { job } | Waiting::BehindCpu { job } => Some(*job),
                _ => None,
            })
            .collect();
        assert!(behind.len() <= 1, "never two barriers: {:?}", plan.waiting);
        assert!(
            behind
                .iter()
                .all(|job| plan.barrier.map(|barrier| barrier.job) == Some(*job)),
            "only the plan's barrier holds a job back: {plan:?}"
        );
        let started: u64 = plan
            .decisions
            .iter()
            .filter_map(|decision| match decision {
                Decision::Start { memory, .. } => Some(*memory),
                Decision::Stop { .. } => None,
            })
            .sum();
        assert!(
            in_use_before + started <= BUDGET,
            "starts fit the budget: {plan:?}"
        );
        if let Some(barrier) = plan.barrier {
            assert!(
                model.memory.iter().any(|memory| *memory <= BUDGET),
                "a barrier stands only for what an empty slice holds"
            );
            assert!(
                !plan.waiting.contains_key(&barrier.job)
                    || !matches!(
                        plan.waiting[&barrier.job],
                        Waiting::BehindMemory { .. } | Waiting::BehindCpu { .. }
                    ),
                "its owner keeps its own reason: {plan:?}"
            );
        }
    }

    /// Nothing starting while a job fits needs a barrier to say why (a stalled queue), and no plan breaks `holds`.
    #[test]
    fn the_barrier_never_stalls_the_queue_over_generated_sequences() {
        for seed in 0..256 {
            let mut rng = Rng(seed);
            let mut model = Model::new();
            for _ in 0..200 {
                match rng.below(10) {
                    0..=3 => model.queue(usize::try_from(rng.below(3)).unwrap()),
                    4 => model.waits = [Waits::default(); 3],
                    5 => {
                        let pool = usize::try_from(rng.below(3)).unwrap();
                        model.memory[pool] = (1 + rng.below(20)) * GIB;
                    }
                    6 => {
                        let pool = usize::try_from(rng.below(3)).unwrap();
                        model.removed[pool] = !model.removed[pool];
                        model.waits[pool] = Waits::default();
                    }
                    _ => {}
                }
                let unlisted = (rng.below(5) == 0).then(|| usize::try_from(rng.below(3)).unwrap());
                let in_use: u64 = model.running.iter().map(|running| running.memory).sum();
                let plan = model.plan(unlisted, &mut rng);
                holds(&model, &plan, in_use);
                let started = !starts(&plan.decisions).is_empty();
                let fits = (0..POOLS.len()).any(|pool| {
                    !model.removed[pool]
                        && unlisted != Some(pool)
                        && !model.queued[pool].is_empty()
                        && in_use + model.memory[pool] <= BUDGET
                });
                assert!(
                    started || !fits || plan.barrier.is_some() || model.running.len() >= 8,
                    "seed {seed}: a job's runner fits, no hold is owed, and nothing started: {plan:?}"
                );
                model.elapse();
            }
        }
    }

    /// With bounded run times, fresh listings and no reload, an aged demand's pool starts a runner once the room
    /// drains: at most the threshold, then one longest run, after its wait began, whatever younger jobs keep arriving.
    #[test]
    fn a_starved_pool_starts_within_a_bound_under_a_stream_of_younger_jobs() {
        let limit = MEMORY_PRIORITY_AFTER.as_secs() / TICK.as_secs();
        for seed in 0..256 {
            let mut rng = Rng(seed);
            let mut model = Model::new();
            // A slice already full of small runners, ending at different times.
            model.queue(2);
            model.queue(2);
            for _ in 0..6 {
                model.queue(2);
            }
            let plan = model.plan(None, &mut rng);
            assert_eq!(starts(&plan.decisions).len(), 8);
            model.queue(0);
            let big = model.next_job - 1;
            let began = model.tick;
            let started = loop {
                model.queue(2);
                model.queue(2);
                let plan = model.plan(None, &mut rng);
                if !model.queued[0].contains(&big) {
                    break model.tick;
                }
                if plan.barrier.is_some() {
                    assert_eq!(
                        starts(&plan.decisions),
                        [],
                        "seed {seed}: nothing younger starts past the barrier"
                    );
                }
                assert!(
                    model.tick <= began + limit + LONGEST + 2,
                    "seed {seed}: the starved pool has not started by tick {}",
                    model.tick
                );
                model.elapse();
            };
            assert!(started <= began + limit + LONGEST + 2);
        }
    }
}
