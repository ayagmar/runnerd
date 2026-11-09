//! A pool's health against the rules it replaced, kept here as the reference: the same runners ending, failing to
//! start and failing to register, with time passing and restarts and reloads between, leave both in the same state and
//! answer the same holds. Only 0, 1 and more than one of an idle streak are compared, as only those differ.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use super::invariants::Rng;
use crate::api::HoldCause;
use crate::scheduler::Hold;
use crate::scheduler::engine::pool_health::{NewHold, PoolHealth};
use crate::scheduler::engine::runner_exit::Logged;
use crate::scheduler::engine::runners::backoff;

const CASES: u64 = 512;
const STEPS: u64 = 80;
const RUNNERS: [&str; 4] = ["a", "b", "c", "d"];

/// The rules as `released` and `hold` kept them over raw counters.
#[derive(Debug, Default)]
struct OldRules {
    held: Option<Hold>,
    failures: u32,
    idle_exits: u32,
    left_beside: BTreeSet<String>,
}

impl OldRules {
    fn exited(
        &mut self,
        logged: Logged,
        runner: &str,
        beside: BTreeSet<String>,
        now: SystemTime,
    ) -> Option<NewHold> {
        match logged {
            Logged::TookJob => {
                self.failures = 0;
                self.idle_exits = 0;
                self.left_beside.clear();
                self.held = None;
                None
            }
            Logged::NoJob { connected: true } => {
                let again = !self.left_beside.remove(runner);
                if again {
                    self.idle_exits += 1;
                    self.left_beside = beside;
                    if self.idle_exits == 1 {
                        self.failures = 0;
                    }
                }
                if again && self.idle_exits >= 2 {
                    self.hold(HoldCause::IdleExits, now)
                } else {
                    None
                }
            }
            Logged::NoJob { connected: false } => self.hold(HoldCause::NoJob, now),
            Logged::Unread => None,
        }
    }

    fn hold(&mut self, cause: HoldCause, now: SystemTime) -> Option<NewHold> {
        if self.held.is_some_and(|held| held.until > now) {
            return None;
        }
        self.failures += 1;
        let wait = backoff(self.failures);
        self.held = Some(Hold {
            until: now + wait,
            cause,
        });
        Some(NewHold {
            wait,
            first: self.failures == 1,
        })
    }
}

fn generated_exit(rng: &mut Rng) -> (Logged, &'static str, BTreeSet<String>) {
    let logged = match rng.below(10) {
        0 => Logged::TookJob,
        1..=5 => Logged::NoJob { connected: true },
        6..=7 => Logged::NoJob { connected: false },
        _ => Logged::Unread,
    };
    let runner = RUNNERS[usize::try_from(rng.below(4)).unwrap()];
    let beside = RUNNERS
        .iter()
        .filter(|other| **other != runner && rng.below(2) == 0)
        .map(|other| (*other).to_owned())
        .collect();
    (logged, runner, beside)
}

fn assert_same(seed: u64, step: u64, new: &PoolHealth, old: &OldRules, now: SystemTime) {
    let held = |hold: Option<Hold>| hold.map(|hold| (hold.until, hold.cause));
    assert_eq!(
        held(new.held()),
        held(old.held),
        "seed {seed} step {step}: the hold"
    );
    assert_eq!(
        new.failures(),
        old.failures,
        "seed {seed} step {step}: failures"
    );
    assert_eq!(
        new.idle_exits(),
        old.idle_exits.min(2),
        "seed {seed} step {step}: idle streak"
    );
    assert_eq!(
        new.holding(now),
        old.held.is_some_and(|held| held.until > now),
        "seed {seed} step {step}: whether it holds now"
    );
}

#[test]
fn a_pool_keeps_the_old_rules_over_generated_exits() {
    for seed in 0..CASES {
        let mut rng = Rng(seed);
        let mut now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut new = PoolHealth::default();
        let mut old = OldRules::default();
        for step in 0..STEPS {
            match rng.below(16) {
                0..=8 => {
                    let (logged, runner, beside) = generated_exit(&mut rng);
                    let cause = new.runner_left(logged, runner, beside.clone());
                    let answer = cause.and_then(|cause| new.hold(cause, now));
                    assert_eq!(
                        answer,
                        old.exited(logged, runner, beside, now),
                        "seed {seed} step {step}: what {runner} ending as {logged:?} holds"
                    );
                }
                9..=10 => {
                    let cause = if rng.below(2) == 0 {
                        HoldCause::Register
                    } else {
                        HoldCause::Start
                    };
                    assert_eq!(
                        new.hold(cause, now),
                        old.hold(cause, now),
                        "seed {seed} step {step}: a hold for {cause:?}"
                    );
                }
                11 => now += Duration::from_secs(rng.below(30)),
                12 => now += Duration::from_secs(700),
                // The instant a hold ends, when it no longer holds.
                13 => now = new.held().map_or(now, |held| held.until),
                14 => {
                    if let Some(held) = new.held() {
                        now = held.until - Duration::from_secs(1);
                    }
                }
                // A restart, or a reload that drops the pool: nothing of either is kept.
                _ => {
                    new = PoolHealth::default();
                    old = OldRules::default();
                }
            }
            assert_same(seed, step, &new, &old, now);
        }
    }
}
