//! Auto sizing's `slots` under measured admission (ADR 0090): raised on waits for a slot alone, as far as Docker's
//! disk holds, and lowered one at a time once slots stayed idle for a week.

use std::collections::BTreeMap;

use super::{G, HOUR, M, NOW, job, measured};
use crate::api::{Resize, SizingChange};
use crate::cache::Disk;
use crate::config::{Config, resize_profile};
use crate::sizing::auto::{Context, IDLE_FOR, changes};
use crate::sizing::{Held, History, Ran, SlotsSet, Window, spread};

const DAY: u64 = 24 * HOUR;

/// `measured()` with auto sizing on, a day between lowerings and node up to `max_slots` 12.
fn config_text() -> String {
    measured().replace("slots = 4", "slots = 4\nmax_slots = 12")
        + "\n[sizing]\nauto = true\ninterval = \"24h\"\nmax_step = 50\n"
}

fn window(now: u64) -> Window {
    Window {
        since: now - 30 * DAY,
        now,
        sample_interval: 2,
    }
}

/// A storefront build in node queued at `queued`, started `waited` ms later, `slot_waited` of it for a slot alone,
/// running 10 minutes.
fn build(queued: u64, waited: u64, slot_waited: u64) -> Ran {
    Ran {
        memory_cap: Some(5632 * M),
        queued_at: queued,
        started_at: Some(queued + waited),
        runner_started_at: queued + waited,
        finished_at: Some(queued + waited + 10 * 60_000),
        slot_waited,
        ..job("halyard/storefront", "build", "node", 2 * G, 0)
    }
}

/// A runner of node holding a slot from `from` to `to` (ms).
fn held(from: u64, to: u64) -> Held {
    Held {
        profile: "node".to_owned(),
        started_at: from,
        removed_at: Some(to),
    }
}

mod idle {
    use super::*;

    /// Node at 4 slots for 20 days, 12 builds a day, `at_once` of them at a time, and nothing waited.
    fn quiet(at_once: u64) -> History {
        let mut history = History::default();
        for day in 1..=20 {
            for i in 0..12 {
                let at = NOW - day * DAY + (i / at_once) * HOUR;
                history.jobs.push(build(at, 0, 0));
                history.runners.push(held(at, at + 10 * 60_000));
            }
        }
        history.slots_set.push(SlotsSet {
            profile: "node".to_owned(),
            slots: 4,
            at: NOW - 20 * DAY,
        });
        history
    }

    fn look(history: &History, running: u32, last: &BTreeMap<String, u64>) -> Vec<SizingChange> {
        let config = Config::parse(&config_text()).unwrap();
        let running = BTreeMap::from([("node".to_owned(), running)]);
        let disk = Disk {
            docker: 1 << 40,
            slots: BTreeMap::new(),
        };
        changes(
            &config,
            history,
            Context {
                memory_max: Some(64 * G),
                cpus: Some(32_000),
                window: window(NOW),
                last,
                cpus_set: &[],
                running: &running,
                disk: Some(&disk),
            },
        )
        .into_iter()
        .filter(|change| change.to.slots != change.from.slots)
        .collect()
    }

    #[test]
    fn slots_idle_for_a_week_lose_one_at_a_time() {
        // Catches slots that only ever go up: each idle one keeps a volume and shrinks every slot's disk share.
        // One build at a time and one warm runner leave 2 of node's 4 slots idle all week.
        let lowered = look(&quiet(1), 0, &BTreeMap::new());
        let [change] = &lowered[..] else {
            panic!("one change: {lowered:?}")
        };
        assert_eq!(change.to.slots, 3);
        assert!(
            change.reason.contains("at most 1 of its 4 slots"),
            "{}",
            change.reason
        );
    }

    #[test]
    fn slots_used_within_two_of_their_number_stay() {
        // Catches a lowering that the next burst takes back: two at a time and a warm runner leave one slot spare.
        assert_eq!(look(&quiet(2), 0, &BTreeMap::new()), Vec::new());
    }

    #[test]
    fn a_hand_raise_stands_for_a_week_and_running_runners_keep_their_slots() {
        // Catches a hand raise taken back at the next look from the week before it, and a lowering under what runs.
        let mut raised = quiet(1);
        raised.slots_set.push(SlotsSet {
            profile: "node".to_owned(),
            slots: 4,
            at: NOW - 2 * DAY,
        });
        raised.slots_set.insert(
            0,
            SlotsSet {
                profile: "node".to_owned(),
                slots: 3,
                at: NOW - 30 * DAY,
            },
        );
        raised.slots_set.retain(|set| set.at != NOW - 20 * DAY);
        assert_eq!(look(&raised, 0, &BTreeMap::new()), Vec::new());
        let changed = BTreeMap::from([("node".to_owned(), (NOW - 3 * DAY) / 1000)]);
        assert_eq!(
            look(&quiet(1), 0, &changed),
            Vec::new(),
            "auto sizing's own change 3 days ago"
        );
        assert_eq!(look(&quiet(1), 4, &BTreeMap::new()), Vec::new());
    }

    #[test]
    fn a_week_with_too_few_jobs_or_no_history_says_nothing() {
        // Catches a host that was off for a week, or a daemon a few days old, losing slots it ran nothing in.
        let mut off = quiet(1);
        off.jobs.retain(|run| run.queued_at < NOW - IDLE_FOR);
        assert_eq!(look(&off, 0, &BTreeMap::new()), Vec::new());
        let young = History {
            runners: quiet(1)
                .runners
                .into_iter()
                .filter(|runner| runner.started_at > NOW - 5 * DAY)
                .collect(),
            ..quiet(1)
        };
        assert_eq!(look(&young, 0, &BTreeMap::new()), Vec::new());
        assert_eq!(look(&History::default(), 0, &BTreeMap::new()), Vec::new());
    }
}

mod generated {
    //! Seeded weeks of node's builds: busy weeks of bursts past its slots, some held back by a slot alone and some by
    //! memory, then quiet weeks; a hand edit lowering slots, an undo, restarts that announce the profiles again, and
    //! runs and events of a profile the config no longer has. Auto sizing looks every hour from what the store keeps.

    use super::*;

    const SEEDS: u64 = 6;
    /// Docker's disk: 48G holds 12 slots of 4G, node's 10 beside jvm's 2.
    const DOCKER: u64 = 48 * G;

    /// A deterministic generator (`SplitMix64`).
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) % n
        }
    }

    struct Daemon {
        text: String,
        history: History,
        /// Auto sizing's last change of each profile, in seconds.
        last: BTreeMap<String, u64>,
    }

    impl Daemon {
        fn slots(&self) -> u32 {
            Config::parse(&self.text).unwrap().profiles["node"].slots
        }

        /// The `profile.changed` events of a start, a reload or a change.
        fn announce(&mut self, now: u64) {
            for (name, profile) in &Config::parse(&self.text).unwrap().profiles {
                self.history.slots_set.push(SlotsSet {
                    profile: name.clone(),
                    slots: profile.slots,
                    at: now,
                });
            }
        }

        /// node's slots written by hand, or by `runnerctl sizing undo`: announced, no change of auto sizing's.
        fn set_by_hand(&mut self, slots: u32, now: u64) {
            let memory = Config::parse(&self.text).unwrap().profiles["node"].memory.0;
            let resize = Resize {
                memory,
                slots,
                cpus: None,
            };
            self.text = resize_profile(&self.text, "node", resize).unwrap();
            self.announce(now);
        }

        /// A burst of `count` builds queued at `at`, as many at once as node's slots, each 10 minutes, the rest
        /// waiting for one: for a slot alone, or with `memory` short too.
        fn burst(&mut self, at: u64, count: u64, memory: bool) {
            let slots = u64::from(self.slots());
            for i in 0..count {
                let waited = (i / slots) * 10 * 60_000;
                let slot_waited = if memory { 0 } else { waited };
                let run = build(at, waited, slot_waited);
                self.history
                    .runners
                    .push(held(at + waited, at + waited + 10 * 60_000));
                self.history.jobs.push(run);
            }
        }

        fn look(&mut self, now: u64, running: u32) -> Vec<SizingChange> {
            self.history
                .jobs
                .retain(|run| run.queued_at >= now - 30 * DAY);
            self.history
                .runners
                .retain(|runner| runner.removed_at.is_none_or(|at| at >= now - 30 * DAY));
            let disk = Disk {
                docker: DOCKER,
                slots: BTreeMap::from([
                    ("node".to_owned(), vec![3 * G; 4]),
                    ("jvm".to_owned(), vec![2 * G; 2]),
                ]),
            };
            let running = BTreeMap::from([("node".to_owned(), running)]);
            let made = changes(
                &Config::parse(&self.text).unwrap(),
                &self.history,
                Context {
                    memory_max: Some(64 * G),
                    cpus: Some(32_000),
                    window: window(now),
                    last: &self.last,
                    cpus_set: &[],
                    running: &running,
                    disk: Some(&disk),
                },
            );
            for change in &made {
                self.text = resize_profile(&self.text, &change.profile, change.to).unwrap();
                self.last.insert(change.profile.clone(), change.at);
            }
            if !made.is_empty() {
                self.announce(now);
            }
            made
        }
    }

    /// The most of `runs` that waited for a slot alone at once, counted at each wait's start.
    fn most_waiting(runs: &[&Ran]) -> u32 {
        let waits: Vec<(u64, u64)> = runs
            .iter()
            .filter(|run| run.slot_waited > 0)
            .map(|run| {
                let started = run.started_at.unwrap();
                (started - run.slot_waited, started)
            })
            .collect();
        waits
            .iter()
            .map(|(at, _)| {
                waits
                    .iter()
                    .filter(|(from, to)| from <= at && at < to)
                    .count()
            })
            .max()
            .map_or(0, |most| u32::try_from(most).unwrap())
    }

    /// The most runners of node that held a slot at once from `start` on.
    fn most_held(history: &History, start: u64, now: u64) -> u32 {
        let runners: Vec<(u64, u64)> = history
            .runners
            .iter()
            .filter(|runner| runner.profile == "node")
            .map(|runner| {
                (
                    runner.started_at.max(start),
                    runner.removed_at.unwrap_or(now),
                )
            })
            .filter(|(from, to)| from < to)
            .collect();
        runners
            .iter()
            .map(|(at, _)| {
                runners
                    .iter()
                    .filter(|(from, to)| from <= at && at < to)
                    .count()
            })
            .max()
            .map_or(0, |most| u32::try_from(most).unwrap())
    }

    /// Checks a change of node's slots against what the store held before the look that made it.
    fn check_change(
        change: &SizingChange,
        before: &History,
        last: Option<u64>,
        running: u32,
        now: u64,
    ) -> Result<(), String> {
        let (from, to) = (change.from.slots, change.to.slots);
        if to > from {
            let start = last.map_or(now - 30 * DAY, |at| at * 1000);
            let fresh: Vec<&Ran> = before
                .jobs
                .iter()
                .filter(|run| run.profile == "node" && run.queued_at >= start)
                .collect();
            let p95 = spread(fresh.iter().map(|run| run.slot_waited / 1000).collect()).p95;
            if fresh.len() < 3 || p95 < 60 {
                return Err(format!(
                    "raised without waits for a slot alone (p95 {p95}s)"
                ));
            }
            let step = (from / 2).max(1);
            if to - from > step || to - from > most_waiting(&fresh).max(1) {
                return Err(format!(
                    "raised by more than {step} or than waited at once ({})",
                    most_waiting(&fresh)
                ));
            }
            if to > 12 || DOCKER / u64::from(to + 2) < 4 * G {
                return Err("raised past max_slots or the disk's floor".to_owned());
            }
        } else if to < from {
            let set = before
                .slots_set
                .iter()
                .filter(|set| set.profile == "node")
                .rev()
                .take_while(|set| set.slots == from)
                .last()
                .map_or(0, |set| set.at);
            let changed = last.map_or(0, |at| at * 1000);
            let start = now - IDLE_FOR;
            if to + 1 != from || to < running.max(1) {
                return Err(format!(
                    "lowered by more than one or under the {running} running"
                ));
            }
            if set.max(changed) > start {
                return Err("lowered within a week of the slots being set".to_owned());
            }
            // One warm runner, and 2 slots spare.
            if most_held(before, start, now) + 1 + 2 > from {
                return Err(format!(
                    "lowered with {} held at once this week",
                    most_held(before, start, now)
                ));
            }
        }
        Ok(())
    }

    fn check(seed: u64) -> Result<Vec<SizingChange>, String> {
        let mut rng = Rng(seed);
        let mut daemon = Daemon {
            text: config_text(),
            history: History::default(),
            last: BTreeMap::new(),
        };
        let start = NOW;
        daemon.announce(start);
        if !daemon.look(start, 0).is_empty() {
            return Err("an empty history changed something".to_owned());
        }
        // A profile the config no longer has: its runs and events are read, and change nothing.
        daemon.history.jobs.push(Ran {
            profile: "gone".to_owned(),
            ..build(start, 600_000, 600_000)
        });
        daemon.history.slots_set.push(SlotsSet {
            profile: "gone".to_owned(),
            slots: 9,
            at: start,
        });
        let mut made = Vec::new();
        for hour in 1..60 * 24 {
            let now = start + hour * HOUR;
            let day = hour / 24;
            let busy = day < 21 || (35..42).contains(&day);
            if busy && rng.below(4) == 0 {
                daemon.burst(now - HOUR / 2, 1 + rng.below(14), rng.below(3) == 0);
            } else if !busy && rng.below(6) == 0 {
                daemon.burst(now - HOUR / 2, 1, false);
            }
            if hour == 10 * 24 {
                let slots = daemon.slots();
                daemon.set_by_hand(slots.saturating_sub(2).max(1), now);
            }
            if hour == 30 * 24
                && let Some(change) = made.iter().rev().find(|change: &&SizingChange| {
                    change.profile == "node" && change.to.slots != change.from.slots
                })
            {
                // `runnerctl sizing undo` of the last slot change.
                let slots = change.from.slots;
                daemon.set_by_hand(slots, now);
            }
            if hour % (5 * 24) == 0 {
                // A restart announces every profile again, as it has it.
                daemon.announce(now);
            }
            let running = u32::try_from(rng.below(u64::from(daemon.slots()) + 1)).unwrap();
            let before = daemon.history.clone();
            let last = daemon.last.get("node").copied();
            for change in daemon.look(now, running) {
                if change.profile == "node" {
                    check_change(&change, &before, last, running, now).map_err(|failure| {
                        format!("seed {seed}, day {day}: {failure}: {change:#?}")
                    })?;
                }
                made.push(change);
            }
        }
        Ok(made)
    }

    #[test]
    fn slots_go_up_only_for_waits_for_a_slot_and_down_only_after_an_idle_week() {
        let mut made = Vec::new();
        for seed in 0..SEEDS {
            made.extend(check(seed).unwrap_or_else(|failure| panic!("{failure}")));
        }
        let node: Vec<&SizingChange> = made
            .iter()
            .filter(|change| change.profile == "node" && change.to.slots != change.from.slots)
            .collect();
        let kinds = [
            node.iter()
                .any(|change| change.to.slots > change.from.slots + 1),
            node.iter()
                .any(|change| change.to.slots < change.from.slots),
            node.iter().any(|change| change.to.slots == 10),
        ];
        assert_eq!(
            kinds, [true; 3],
            "raises of more than one, lowerings, and the disk's bound reached: {node:#?}"
        );
        assert!(
            made.iter().all(|change| change.profile != "gone"),
            "nothing changes a profile the config no longer has"
        );
    }
}
