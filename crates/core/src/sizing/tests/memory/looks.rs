//! Auto sizing's looks over generated days, as the daemon runs them: each change written, recorded and run from
//! then. Each look reads only what the store keeps (the runs, auto sizing's changes, the `profile.changed` events),
//! so a daemon restarted between two looks decides as one that kept running.

use std::collections::BTreeMap;

use super::{DAY, G, HOUR, M, NOW, failed, jvm_at, ledger, met, window};
use crate::api::{self, SizingChange};
use crate::config::{Config, resize_profile};
use crate::sizing::auto::{Context, next_look};
use crate::sizing::memory::{CHANGES_READ, HISTORY};
use crate::sizing::{CpusSet, History, Ran};

/// What the daemon keeps between looks: the config file and the history.
struct Daemon {
    text: String,
    history: History,
    cpus_set: Vec<CpusSet>,
    now: u64,
    /// The slice's `MemoryMax`.
    memory_max: u64,
}

impl Daemon {
    /// Started at `now` with `text`.
    fn new(text: String, now: u64) -> Daemon {
        let mut daemon = Daemon {
            text,
            history: History::default(),
            cpus_set: Vec::new(),
            now,
            memory_max: 64 * G,
        };
        daemon.announce();
        daemon
    }

    fn config(&self) -> Config {
        Config::parse(&self.text).unwrap()
    }

    /// jvm's `memory` now.
    fn cap(&self) -> u64 {
        self.config().profiles["jvm"].memory.0
    }

    /// The `profile.changed` events of a start or a reload.
    fn announce(&mut self) {
        for (name, profile) in &self.config().profiles {
            self.history.memory_set.push(crate::sizing::MemorySet {
                profile: name.clone(),
                memory: profile.memory.0,
                at: self.now,
            });
            self.cpus_set.push(CpusSet {
                profile: name.clone(),
                cpus: profile.cpus,
                at: self.now,
            });
        }
    }

    /// jvm's `memory` set to `memory` by hand and the daemon reloaded: announced, but no change of auto sizing's.
    fn set_by_hand(&mut self, memory: u64) {
        let slots = self.config().profiles["jvm"].slots;
        let resize = api::Resize {
            memory,
            slots,
            cpus: None,
        };
        self.text = resize_profile(&self.text, "jvm", resize).unwrap();
        self.announce();
    }

    /// A look at `now`: the changes due, each written and recorded.
    fn look(&mut self, now: u64) -> Vec<SizingChange> {
        self.now = now;
        // What the daemon reads: 30 days of runs, 60 of changes.
        self.history
            .jobs
            .retain(|run| run.queued_at >= now - HISTORY);
        self.history
            .cancelled
            .retain(|run| run.queued_at >= now - HISTORY);
        self.history
            .changes
            .retain(|change| change.at * 1000 >= now - CHANGES_READ);
        let mut last = BTreeMap::new();
        for change in &self.history.changes {
            last.insert(change.profile.clone(), change.at);
        }
        let look = next_look(
            &self.config(),
            &self.history,
            Context {
                memory_max: Some(self.memory_max),
                cpus: Some(32_000),
                window: window(now),
                last: &last,
                cpus_set: &self.cpus_set,
                running: &BTreeMap::new(),
            },
        );
        for change in &look.changes {
            self.text = resize_profile(&self.text, &change.profile, change.to).unwrap();
            self.history.changes.push(change.clone());
        }
        if !look.changes.is_empty() {
            self.announce();
        }
        look.changes
    }
}

/// A raise that takes back a lowering.
fn is_revert(change: &SizingChange) -> bool {
    change.reason.starts_with("raise memory back to")
}

mod days {
    //! The ledger's tests in a JVM whose heap grows toward 63% of its cap, needing 5G, and 7G when a cold cache
    //! makes one run rebuild everything.

    use std::collections::BTreeMap;

    use super::{DAY, Daemon, G, HOUR, M, NOW, SizingChange, is_revert, jvm_at, ledger, met};
    use crate::sizing::memory::HISTORY;

    /// The look every 6 hours, with 3 runs before each.
    const LOOK: u64 = 6 * HOUR;

    /// The run at `cap` of a job that needs `need`: its heap fills 63% of the cap, and it meets a cap under `need`.
    fn ran(at: u64, cap: u64, need: u64, run_id: u64) -> crate::sizing::Ran {
        let run = crate::sizing::Ran {
            run_id,
            branch: Some(
                if run_id.is_multiple_of(2) {
                    "main"
                } else {
                    "fix/parser"
                }
                .to_owned(),
            ),
            ..ledger(at, cap, need.max(cap / 100 * 63))
        };
        if need > cap { met(run) } else { run }
    }

    /// Days of runs and looks from 14G × 2, with a cold cache once every `cold` runs: when runs met the cap, and the
    /// changes.
    fn generate(days: u64, cold: u64) -> (Vec<(u64, u64)>, Vec<SizingChange>) {
        let mut daemon = Daemon::new(jvm_at("14G", 2), NOW);
        let mut met_at = Vec::new();
        let mut all = Vec::new();
        let mut run_id = 0;
        let mut now = NOW;
        while now < NOW + days * DAY {
            let cap = daemon.cap();
            for i in 0..3 {
                run_id += 1;
                let need = if run_id % cold == 0 { 7 * G } else { 5 * G };
                let run = ran(now + i * HOUR, cap, need, run_id);
                if run.own_cap == Some(true) {
                    met_at.push((now + i * HOUR, cap));
                }
                daemon.history.jobs.push(run);
            }
            now += LOOK;
            all.extend(daemon.look(now));
        }
        (met_at, all)
    }

    #[test]
    fn an_elastic_jvm_is_lowered_to_what_it_needs_and_each_lowering_a_cold_run_meets_is_reverted_once()
     {
        for cold in [37, 100, 250] {
            let (met_at, changes) = generate(120, cold);
            let jvm: Vec<&SizingChange> = changes.iter().filter(|c| c.profile == "jvm").collect();
            assert!(
                jvm.last().is_some_and(|change| change.to.memory <= 9 * G),
                "cold 1 in {cold}: lowered from 14G toward what it needs: {jvm:#?}"
            );
            assert!(
                cold == 37 || !met_at.is_empty(),
                "cold 1 in {cold}: a lowering under the cold runs' need, met once: {jvm:#?}"
            );
            for change in &jvm {
                if change.to.memory > change.from.memory {
                    assert!(
                        is_revert(change),
                        "cold 1 in {cold}: only a revert raises an elastic JVM: {change:#?}"
                    );
                }
            }
            let mut per_lowering = BTreeMap::new();
            for &(at, cap) in &met_at {
                // The lowering it ran under, and the look after it.
                let lowering = jvm
                    .iter()
                    .rposition(|change| change.at * 1000 <= at)
                    .unwrap();
                assert!(
                    jvm[lowering].to.memory == cap && jvm[lowering].from.memory > cap,
                    "cold 1 in {cold}: only a lowering's cap is met: {:#?}",
                    jvm[lowering]
                );
                *per_lowering.entry(lowering).or_insert(0) += 1;
                let from = jvm[lowering].from.memory;
                let next = jvm[lowering + 1];
                assert!(
                    next.at * 1000 <= at + LOOK && next.to.memory == from,
                    "cold 1 in {cold}: the look after a run met {}M reverts to {}M: {next:#?}",
                    cap / M,
                    from / M
                );
                for later in &jvm[lowering + 1..] {
                    if later.at * 1000 < at + HISTORY {
                        assert!(
                            later.to.memory >= from,
                            "cold 1 in {cold}: nothing under {}M within 30 days of a revert: {later:#?}",
                            from / M
                        );
                    }
                }
            }
            assert!(
                per_lowering.values().all(|&runs| runs == 1),
                "cold 1 in {cold}: one run meets each lowering at most: {per_lowering:?}"
            );
            assert!(
                met_at.len() <= 5,
                "cold 1 in {cold}: at most one burst in each 30 days: {met_at:?}"
            );
        }
    }
}

/// How the lowered profile of case 10 shows it went too far.
#[derive(Debug, Clone, Copy)]
enum TooFar {
    /// A run meets the lowered cap.
    Met,
    /// The ledger's test fails 3 times in a row at it, on two branches, with no kernel event.
    Failed,
}

/// The ledger's test every 2 hours from `start`, `count` times, peaking at `peak` under whatever cap jvm has, a look
/// after each; the changes.
fn every_two_hours(daemon: &mut Daemon, start: u64, count: u64, peak: u64) -> Vec<SizingChange> {
    let mut changes = Vec::new();
    for i in 0..count {
        let at = start + i * 2 * HOUR;
        let cap = daemon.cap();
        daemon.history.jobs.push(ledger(at, cap, peak));
        changes.extend(daemon.look(at + HOUR));
    }
    changes
}

#[test]
fn a_profile_steps_down_to_its_jobs_need_stops_there_and_takes_back_the_step_that_went_too_far() {
    for too_far in [TooFar::Met, TooFar::Failed] {
        let mut daemon = Daemon::new(jvm_at("14G", 2), NOW);
        // An old run peaked 13.5G, clear of 14G; the test peaks 7G now, which needs 8.25G.
        daemon.history.jobs.push(ledger(NOW, 14 * G, 13824 * M));
        let lowered = every_two_hours(&mut daemon, NOW + HOUR, 180, 7 * G);
        let steps: Vec<(u64, u64)> = lowered
            .iter()
            .map(|change| (change.from.memory, change.to.memory))
            .collect();
        assert_eq!(
            steps,
            [(14 * G, 10752 * M), (10752 * M, 8448 * M)],
            "{too_far:?}: 25% down, then to the need, and no further"
        );
        // The 13.5G run holds 14G until 60 successful runs came after it, 2 hours apart.
        assert!(
            lowered[0].at * 1000 >= NOW + 120 * HOUR,
            "{too_far:?}: {:?}",
            lowered[0]
        );
        assert!(
            lowered[1].at - lowered[0].at >= 24 * 3600,
            "{too_far:?}: a lowering a day"
        );
        let at = NOW + 400 * HOUR;
        match too_far {
            TooFar::Met => daemon.history.jobs.push(met(ledger(at, 8448 * M, 0))),
            TooFar::Failed => {
                for (i, branch) in [(0, "main"), (1, "fix/parser"), (2, "main")] {
                    daemon.history.jobs.push(failed(
                        ledger(at + i * HOUR, 8448 * M, 7 * G),
                        branch,
                        at + i,
                    ));
                }
            }
        }
        let reverted = daemon.look(at + 3 * HOUR);
        assert_eq!(
            reverted
                .iter()
                .map(|change| (change.from.memory, change.to.memory))
                .collect::<Vec<_>>(),
            [(8448 * M, 10752 * M)],
            "{too_far:?}: straight back to the cap that step came from"
        );
        let after = every_two_hours(&mut daemon, at + 4 * HOUR, 330, 7 * G);
        for change in &after {
            if change.at * 1000 < at + HISTORY {
                assert!(
                    change.to.memory >= 10752 * M,
                    "{too_far:?}: nothing under 10.5G for 30 days: {change:?}"
                );
            }
        }
    }
}

#[test]
fn a_job_heavy_once_in_30_runs_keeps_the_cap_its_heavy_run_needs() {
    let mut daemon = Daemon::new(jvm_at("9G", 2), NOW);
    let mut changes = Vec::new();
    for i in 0..480 {
        let at = NOW + i * 2 * HOUR;
        // A cold cache rebuilds everything: 7.5G, which needs 8.75G of the 9G.
        let peak = if i % 30 == 0 { 7680 * M } else { 3 * G };
        let cap = daemon.cap();
        daemon.history.jobs.push(ledger(at, cap, peak));
        changes.extend(daemon.look(at + HOUR));
    }
    assert!(
        changes
            .iter()
            .all(|change| change.to.memory == change.from.memory),
        "its heavy run stays in the 60-run window: {changes:#?}"
    );
}

mod generated {
    //! Seeded sequences of runs of three jobs (an elastic JVM test, a cold-cache build, a lint), killed at the cap,
    //! by the slice, at a service container's limit, failing on one broken branch, re-run, lost, cancelled, without
    //! the local counter (killed, in the first days), slow to start (so slots go up) and CPU-bound (so `cpus` go up), with a hand edit of jvm's
    //! `memory` under the cold build's need twice, and auto sizing looking every 3 hours. Invariants in ADR 0070's
    //! terms, after each look.

    use super::{DAY, Daemon, G, HOUR, M, NOW, Ran, SizingChange, api, is_revert, jvm_at, ledger};
    use crate::sizing::memory::HISTORY;

    const DAYS: u64 = 90;
    /// The days the owner sets jvm's `memory` by hand, the first while killed runs recorded before the local counter
    /// are in its windows.
    const HAND_EDITS: [u64; 2] = [12, 70];
    const SEEDS: u64 = 4;

    /// A deterministic generator (`SplitMix64`).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A run at `at` under `cap` and `cpus`.
    fn generate(rng: &mut Rng, at: u64, cap: u64, cpus: u32, run_id: u64) -> (Ran, bool) {
        let (name, need) = match rng.below(3) {
            // A JVM's heap fills 63% of its cap; it needs 5G.
            0 => ("test", 5 * G),
            // A build needing 7G once its cache is cold, 3G otherwise.
            1 => ("build", if rng.below(100) == 0 { 7 * G } else { 3 * G }),
            _ => ("lint", G + rng.below(1024) * M),
        };
        let peak = if name == "test" {
            need.max(cap / 100 * 63)
        } else {
            need
        };
        let cpu_cap = u64::from(cpus) * 1000;
        let mut run = Ran {
            name: name.to_owned(),
            run_id,
            branch: Some(
                if rng.below(2) == 0 {
                    "main"
                } else {
                    "fix/parser"
                }
                .to_owned(),
            ),
            started_at: Some(at + rng.below(300) * 1000),
            cpu_cap: Some(cpu_cap),
            cpu_peak: cpu_cap,
            // Builds are bound by their CPUs; the rest burst.
            cpu_average: Some(if name == "build" {
                cpu_cap * (75 + rng.below(20)) / 100
            } else {
                cpu_cap * (20 + rng.below(40)) / 100
            }),
            ..ledger(at, cap, peak.min(cap))
        };
        if need > cap {
            run = super::met(run);
        }
        let mut cancelled = false;
        match rng.below(20) {
            // Killed while its own limit held: the slice ran out.
            0 => {
                run.oom_killed = true;
                run.memory_peak = cap / 3;
                run.succeeded = false;
                run.failed = true;
            }
            // A service container hit its own limit.
            1 => {
                run.oom_killed = true;
                run.hit_cap = Some(true);
            }
            // Red CI on one broken branch, its failed jobs re-run.
            2 | 3 => {
                run.succeeded = false;
                run.failed = true;
                run.branch = Some("fix/broken".to_owned());
                run.run_id = 7;
            }
            4 => {
                run.succeeded = false;
                run.failed = false;
            }
            5 => {
                run.succeeded = false;
                cancelled = true;
            }
            6 => {
                run.own_cap = None;
                run.hit_cap = None;
            }
            // Killed, recorded by a daemon before the local counter, in the first days.
            7 if at < NOW + 10 * DAY => {
                run.own_cap = None;
                run.hit_cap = None;
                run.oom_killed = true;
            }
            _ => {}
        }
        (run, cancelled)
    }

    /// The rows of the history in the 30 days, in the order they finished.
    fn rows(history: &crate::sizing::History, now: u64) -> Vec<(&Ran, bool)> {
        let mut rows: Vec<(&Ran, bool)> = history
            .jobs
            .iter()
            .map(|run| (run, false))
            .chain(history.cancelled.iter().map(|run| (run, true)))
            .filter(|(run, _)| run.queued_at >= now - HISTORY)
            .collect();
        rows.sort_by_key(|(run, _)| run.finished_at);
        rows
    }

    /// The cap a run's container had.
    fn cap(run: &Ran) -> u64 {
        run.memory_cap.unwrap()
    }

    /// What a lowering must stay at or above: over each job's last 60 successful runs and the failed ones among
    /// them, at least its cap for one that met it, else its peak's need but no more than its cap.
    fn lower_floor(rows: &[(&Ran, bool)]) -> u64 {
        let mut floor = 0;
        for name in ["test", "build", "lint"] {
            let runs: Vec<&Ran> = rows
                .iter()
                .filter(|(run, cancelled)| {
                    run.name == name && (!cancelled || run.capped(cap(run)).met())
                })
                .map(|(run, _)| *run)
                .collect();
            let successes: Vec<usize> = (0..runs.len()).filter(|&i| runs[i].succeeded).collect();
            let start = successes.len().checked_sub(60).map_or(0, |i| successes[i]);
            for run in &runs[start..] {
                let need = if run.capped(cap(run)).met() {
                    api::need(run.memory_peak.max(cap(run)), 15)
                } else {
                    api::need(run.memory_peak, 15).min(cap(run))
                };
                floor = floor.max(need);
            }
        }
        floor
    }

    /// Whether a job is on its cap: fewer than 3 successful runs clear of their cap came after its latest run that
    /// met its cap or was killed for memory.
    fn on_cap(rows: &[(&Ran, bool)]) -> bool {
        ["test", "build", "lint"].iter().any(|name| {
            let runs: Vec<&(&Ran, bool)> =
                rows.iter().filter(|(run, _)| run.name == *name).collect();
            runs.iter()
                .rposition(|(run, _)| run.capped(cap(run)).holds())
                .is_some_and(|event| {
                    runs[event + 1..]
                        .iter()
                        .filter(|(run, cancelled)| !cancelled && run.succeeded)
                        .count()
                        < 3
                })
        })
    }

    /// When jvm took `memory`, by the events: the first of the latest run of them with it.
    fn took(history: &crate::sizing::History, memory: u64) -> u64 {
        let set: Vec<_> = history
            .memory_set
            .iter()
            .filter(|set| set.profile == "jvm")
            .collect();
        let last_other = set.iter().rposition(|set| set.memory != memory);
        set[last_other.map_or(0, |i| i + 1)..]
            .first()
            .map_or(0, |set| set.at)
    }

    fn check(seed: u64) -> Result<Vec<SizingChange>, String> {
        let mut rng = Rng(seed);
        let mut daemon = Daemon::new(jvm_at("14G", 2), NOW);
        let mut run_id = 100;
        let mut changes: Vec<SizingChange> = Vec::new();
        let mut now = NOW;
        while now < NOW + DAYS * DAY {
            if HAND_EDITS.iter().any(|day| now == NOW + day * DAY) {
                // The owner sets jvm to 6G by hand, under the cold build's need; the daemon runs it from then.
                daemon.set_by_hand(6 * G);
            }
            let profile = daemon.config().profiles["jvm"].clone();
            for _ in 0..rng.below(4) {
                run_id += 1;
                let at = now + rng.below(3 * 60) * 60_000;
                let (run, cancelled) =
                    generate(&mut rng, at, profile.memory.0, profile.cpus, run_id);
                if cancelled {
                    daemon.history.cancelled.push(run);
                } else {
                    daemon.history.jobs.push(run);
                }
            }
            now += 3 * HOUR;
            let before = daemon.history.clone();
            let rows = rows(&before, now);
            let floor = lower_floor(&rows);
            let held = on_cap(&rows);
            for change in daemon.look(now) {
                let context = format!("seed {seed}, day {}: {change:#?}", (now - NOW) / DAY);
                let (from, to) = (change.from.memory, change.to.memory);
                if change.profile != "jvm" {
                    continue;
                }
                if to < from && (to < floor || held) {
                    return Err(format!(
                        "lowered under {}M or while on its cap: {context}",
                        floor / M
                    ));
                }
                if to > from && !is_revert(&change) {
                    let since = took(&before, from);
                    let met = rows.iter().any(|(run, _)| {
                        run.memory_cap == Some(from)
                            && run.finished_at.unwrap() >= since
                            && run.capped(from) == crate::sizing::Capped::Met
                    });
                    if !met {
                        return Err(format!(
                            "raised without a run that met {}M: {context}",
                            from / M
                        ));
                    }
                    // Back at most to a cap a job ran at, or to what the run that met the cap needs above it.
                    let ran_at = rows
                        .iter()
                        .filter_map(|(run, _)| run.memory_cap)
                        .max()
                        .unwrap_or(0);
                    if to > ran_at.max(api::need(from, 15)) {
                        return Err(format!(
                            "raised above {}M, the largest cap run at: {context}",
                            ran_at / M
                        ));
                    }
                }
                if let Some(revert) = before.changes.iter().rfind(|c| c.profile == "jvm")
                    && !is_revert(&change)
                {
                    let apart = change.at - revert.at;
                    let lowers = to < from || change.to.cpus < change.from.cpus;
                    if apart < 3600 || (lowers && apart < 24 * 3600) {
                        return Err(format!("{apart} s after the last change: {context}"));
                    }
                }
                if change.reason.contains("times in a row") {
                    // At least 3 workflow runs on 2 branches failed at the lowered cap: never one broken branch or
                    // re-runs of one workflow run.
                    let failures: Vec<&Ran> = rows
                        .iter()
                        .filter(|(run, cancelled)| {
                            !cancelled && run.failed && run.memory_cap == Some(from)
                        })
                        .map(|(run, _)| *run)
                        .collect();
                    let runs: std::collections::BTreeSet<u64> =
                        failures.iter().map(|run| run.run_id).collect();
                    let branches: std::collections::BTreeSet<&Option<String>> =
                        failures.iter().map(|run| &run.branch).collect();
                    if runs.len() < 3 || branches.len() < 2 {
                        return Err(format!("reverted on red CI of one branch: {context}"));
                    }
                }
                changes.push(change);
            }
        }
        Ok(changes)
    }

    #[test]
    fn auto_sizing_lowers_only_to_what_the_runs_need_and_raises_only_on_a_run_that_met_the_cap() {
        let mut changes = Vec::new();
        for seed in 0..SEEDS {
            match check(seed) {
                Ok(made) => changes.extend(made),
                Err(failure) => panic!("{failure}"),
            }
        }
        // The sequences reach every kind of change between their runs.
        let kinds = [
            changes.iter().any(|c| c.to.memory < c.from.memory),
            changes
                .iter()
                .any(|c| c.to.memory > c.from.memory && !is_revert(c)),
            changes.iter().any(is_revert),
            changes.iter().any(|c| c.to.slots != c.from.slots),
            changes.iter().any(|c| c.to.cpus != c.from.cpus),
        ];
        assert_eq!(
            kinds, [true; 5],
            "lowerings, raises, reverts, slots and cpus: {changes:#?}"
        );
    }
}

mod ceiling;
