//! Predictions from a job's last runs (ADR 0070): a job that got lighter reserves its new peaks within a window of
//! runs, a hold at the cap ends after clear runs, and the invariants over generated sequences of runs, including a
//! daemon that rebuilds its predictor from the stored rows.

use super::{G, M, MINUTE, NOW, Ran, build, key, predictor, run};
use crate::admission::recent::{CLEAN_RUNS, RECENT_RUNS};
use crate::admission::{Copies, Predictor, UNPREDICTABLE_FOR};
use crate::api::{Basis, JobPeak};
use crate::config::Config;
use crate::sizing::{Capped, History, Measure};

/// A build that finished `minutes` after `NOW`, peaking at `peak` under a 7G cap.
fn at(minutes: u64, peak: u64) -> Ran {
    let queued = NOW + minutes * MINUTE;
    Ran {
        queued_at: queued,
        started_at: Some(queued),
        finished_at: Some(queued + MINUTE / 2),
        ..build(peak)
    }
}

/// `run` with its container's own limit having failed reclaim.
fn met(run: Ran) -> Ran {
    Ran {
        own_cap: Some(true),
        hit_cap: Some(true),
        succeeded: false,
        failed: true,
        ..run
    }
}

fn learned(runs: &[Ran]) -> Predictor {
    let mut predictor = predictor();
    for run in runs {
        predictor.learn(run, "node", 7 * G, run.finished_at.unwrap());
    }
    predictor
}

fn predicted(predictor: &Predictor, now: u64) -> crate::admission::Prediction {
    predictor.predict(&key("build"), "node", 7 * G, now, Copies::Alone)
}

#[test]
fn a_job_that_got_lighter_reserves_its_new_peaks_within_a_window_not_a_week() {
    // The trellis case: hundreds of runs at 5.5G and one at the 7G cap, then the workflow got lighter: 3.5G.
    let mut runs: Vec<Ran> = (0..300).map(|i| at(i, 5632 * M)).collect();
    runs.push(met(at(300, 7 * G)));
    let mut minute = 301;
    let mut light = |count: u64| {
        let more: Vec<Ran> = (0..count).map(|i| at(minute + i, 3584 * M)).collect();
        minute += count;
        more
    };
    runs.extend(light(2));
    let now = runs.last().unwrap().finished_at.unwrap() + MINUTE;
    assert!(
        matches!(
            predicted(&learned(&runs), now).basis,
            Basis::Unpredictable { .. }
        ),
        "two clear runs do not end the hold"
    );
    runs.extend(light(1));
    let after_three = predicted(&learned(&runs), now + 3 * MINUTE);
    // Released: the p95 of its last 20 successful runs and the failed one at the cap among them, still the 5.5G
    // ones: 6.5G.
    assert_eq!(
        (after_three.basis, after_three.memory),
        (
            Basis::Job {
                runs: RECENT_RUNS + 1,
                peak: JobPeak::P95
            },
            6656 * M
        )
    );
    runs.extend(light(16));
    let now = runs.last().unwrap().finished_at.unwrap() + MINUTE;
    assert_eq!(
        predicted(&learned(&runs), now).memory,
        6656 * M,
        "19 light runs leave one at 5.5G in the window, beside the one at the cap"
    );
    runs.extend(light(1));
    let now = runs.last().unwrap().finished_at.unwrap() + MINUTE;
    // 20 light runs: 3.5G + 15% = 4.25G.
    assert_eq!(predicted(&learned(&runs), now).memory, 4352 * M);
}

#[test]
fn a_job_that_alternates_light_runs_and_runs_at_its_cap_stays_on_its_cap() {
    let mut runs = Vec::new();
    for i in 0..60 {
        runs.push(if i % 2 == 0 {
            at(i, 2 * G)
        } else {
            met(at(i, 7 * G))
        });
        let now = runs.last().unwrap().finished_at.unwrap() + MINUTE;
        if i > 0 {
            assert_eq!(
                predicted(&learned(&runs), now).memory,
                7 * G,
                "after run {i}"
            );
        }
    }
}

#[test]
fn a_run_that_met_an_older_smaller_cap_counts_at_it_and_clear_runs_of_the_new_cap_end_the_hold() {
    let mut predictor = predictor();
    for i in 0..10 {
        let lint = run("halyard/storefront", "lint", "node", NOW, 1, G);
        predictor.learn(&lint, "node", 7 * G, NOW + i);
    }
    // It ran out of memory at an older 5G cap, its last sample at 4G.
    let old = Ran {
        memory_cap: Some(5 * G),
        ..met(at(0, 4 * G))
    };
    predictor.learn(&old, "node", 5 * G, NOW);
    for i in 1..=u64::from(CLEAN_RUNS) {
        predictor.learn(&at(i, 3 * G), "node", 7 * G, NOW + i * MINUTE);
    }
    let prediction = predicted(&predictor, NOW + 9 * MINUTE);
    // Released after 3 clear runs at 7G; the old run counts at its 5G cap, not its 4G sample: 5G + 15%.
    assert_eq!(
        prediction,
        crate::admission::Prediction {
            memory: 5888 * M,
            basis: Basis::Job {
                runs: 4,
                peak: JobPeak::Largest
            }
        }
    );
}

#[test]
fn a_kill_a_jobs_own_service_container_explains_holds_nothing() {
    let mut predictor = predictor();
    for i in 0..20 {
        predictor.learn(&at(i, 2 * G), "node", 7 * G, NOW + i * MINUTE);
    }
    // A test container capped at 512M was killed at its own limit: the counters show a limit below the runner's.
    let inner = Ran {
        oom_killed: true,
        hit_cap: Some(true),
        own_cap: Some(false),
        ..at(20, 2 * G)
    };
    assert_eq!(inner.capped(7 * G), Capped::Clear);
    predictor.learn(&inner, "node", 7 * G, NOW + 20 * MINUTE);
    let prediction = predicted(&predictor, NOW + 30 * MINUTE);
    assert!(prediction.memory < 7 * G, "{prediction:?}");
}

#[test]
fn a_run_past_what_it_reserved_holds_its_peak_until_it_leaves_the_window() {
    let mut runs: Vec<Ran> = (0..20).map(|i| at(i, 2 * G)).collect();
    runs.push(Ran {
        reserved: Some(2560 * M),
        ..at(20, 4 * G)
    });
    for more in 1..=20 {
        runs.push(at(20 + more, 2 * G));
        let now = runs.last().unwrap().finished_at.unwrap() + MINUTE;
        let prediction = predicted(&learned(&runs), now);
        if more < 20 {
            assert_eq!(
                prediction.memory,
                4864 * M,
                "4G + 15% while the overrun is among the last 20: {more}"
            );
        } else {
            assert_eq!(prediction.memory, 2560 * M, "out of the window: {more}");
        }
    }
}

/// The invariants over generated sequences of runs. The sequences come from a seeded generator, so a failure replays:
/// it prints its seed and the runs up to it.
mod invariants {
    use std::fmt::Write as _;

    use super::*;
    use crate::admission::recent::finished;
    use crate::admission::{Key, Policy, simulate};

    const CASES: u64 = 48;
    const RUNS: u64 = 160;
    const NAMES: [&str; 3] = ["build", "e2e", "lint"];

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

    /// One generated row: the run and whether it was cancelled.
    #[derive(Debug, Clone)]
    struct Row {
        run: Ran,
        cancelled: bool,
    }

    /// A run of a random job at a random time after `at`, under a cap that grows from 7G to 9G halfway: light, heavy,
    /// at its cap, killed for memory, killed at a service container's limit, failed, cancelled, cancelled at its cap,
    /// past what it reserved, sampled with page cache, or without the local counter. A run finishing with another,
    /// `tied`, is of another job: the order of two runs of one job that finished in the same millisecond is the
    /// store's.
    fn generate(rng: &mut Rng, index: u64, at: u64, tied: Option<&str>) -> Row {
        let cap = if index < RUNS / 2 { 7 * G } else { 9 * G };
        let pick = usize::try_from(rng.below(3)).unwrap();
        let name = match tied.and_then(|tied| NAMES.iter().position(|name| *name == tied)) {
            Some(other) => NAMES[(other + 1 + pick % 2) % 3],
            None => NAMES[pick],
        };
        let peak = G + rng.below(cap / M - 1024) * M;
        let mut run = Ran {
            memory_cap: Some(cap),
            queued_at: at,
            started_at: Some(at),
            finished_at: Some(at + MINUTE),
            reserved: Some(peak + rng.below(2) * G),
            ..run("halyard/storefront", name, "node", at, 1, peak)
        };
        let mut cancelled = false;
        match rng.below(16) {
            0 => run = met(run),
            1 => run.oom_killed = true,
            2 => {
                run.oom_killed = true;
                run.hit_cap = Some(true);
            }
            3 => {
                run.succeeded = false;
                run.failed = true;
            }
            4 => cancelled = true,
            5 => {
                run = met(run);
                cancelled = true;
            }
            6 => run.reserved = Some(peak / 2),
            7 => run.measure = Measure::WithActiveFile,
            8 => {
                run.own_cap = None;
                run.hit_cap = None;
            }
            _ => {}
        }
        if cancelled {
            run.succeeded = false;
        }
        Row { run, cancelled }
    }

    /// The rows as the store returns them: in an order of its own, not the order they finished.
    fn history(rows: &[Row], rng: &mut Rng) -> History {
        let mut shuffled: Vec<&Row> = rows.iter().collect();
        for i in (1..shuffled.len()).rev() {
            let j = usize::try_from(rng.below(i as u64 + 1)).unwrap();
            shuffled.swap(i, j);
        }
        History {
            jobs: shuffled
                .iter()
                .filter(|row| !row.cancelled)
                .map(|row| row.run.clone())
                .collect(),
            cancelled: shuffled
                .iter()
                .filter(|row| row.cancelled)
                .map(|row| row.run.clone())
                .collect(),
            ..History::default()
        }
    }

    /// Whether a run is a cap event, in ADR 0070's words: its container's own limit failed reclaim, or a process in it
    /// was killed for memory while no limit below it was hit; recorded without the local counter, a kill, a limit hit
    /// in the container or, without events, a peak within 5% of the cap.
    fn cap_event(run: &Ran, cap: u64) -> bool {
        match run.own_cap {
            Some(own) => own || (run.oom_killed && run.hit_cap != Some(true)),
            None => run.oom_killed || run.hit_cap.unwrap_or(run.memory_peak * 100 >= cap * 95),
        }
    }

    /// The oracle's hold at the cap of each job: the latest run that met its cap or was killed for memory, and the
    /// successful clear runs in a row since.
    #[derive(Debug, Default, Clone, Copy)]
    struct Hold {
        at: Option<u64>,
        clean: u32,
    }

    fn check(seed: u64) -> Result<(), String> {
        let config = Config::parse(super::super::CONFIG).unwrap();
        let mut rng = Rng(seed);
        let mut rows = Vec::new();
        let mut incremental = Predictor::new(&config.sizing);
        let mut holds: std::collections::BTreeMap<&str, Hold> = std::collections::BTreeMap::new();
        let mut log = String::new();
        let mut store = Rng(!seed);
        let mut at = NOW;
        let mut tied = None;
        for index in 0..RUNS {
            // One run in four finishes with the one before, unless that one did.
            tied = match (tied, rows.last()) {
                (None, Some(Row { run, .. })) if rng.below(4) == 0 => Some(run.name.clone()),
                _ => {
                    at += (1 + rng.below(3 * 24 * 60)) * MINUTE;
                    None
                }
            };
            let row = generate(&mut rng, index, at, tied.as_deref());
            let cap = row.run.memory_cap.unwrap();
            let _ = writeln!(log, "{index}: {row:?}");
            if row.cancelled {
                incremental.mark(&row.run, "node", cap, finished(&row.run));
            } else {
                incremental.learn(&row.run, "node", cap, finished(&row.run));
            }
            let name = NAMES
                .iter()
                .find(|name| **name == row.run.name)
                .copied()
                .unwrap();
            let hold = holds.entry(name).or_default();
            if row.run.measure == Measure::WithActiveFile {
                if row.run.oom_killed {
                    *hold = Hold {
                        at: Some(finished(&row.run)),
                        clean: 0,
                    };
                }
            } else if cap_event(&row.run, cap) {
                *hold = Hold {
                    at: Some(finished(&row.run)),
                    clean: 0,
                };
            } else if !row.cancelled && row.run.succeeded {
                let overran = row.run.reserved.is_some_and(|r| row.run.memory_peak > r);
                hold.clean = if overran { 0 } else { hold.clean + 1 };
            }
            rows.push(row);
            let rebuilt =
                Predictor::read(&config.sizing, &history(&rows, &mut store), |_| Some(cap));
            let now = at + 2 * MINUTE;
            for name in NAMES {
                let key = Key {
                    repository: "halyard/storefront".to_owned(),
                    workflow: Some("CI".to_owned()),
                    name: name.to_owned(),
                };
                let one = incremental.predict(&key, "node", 9 * G, now, Copies::Alone);
                let other = rebuilt.predict(&key, "node", 9 * G, now, Copies::Alone);
                if one != other {
                    return Err(format!(
                        "{log}{name}: run by run {one:?}, rebuilt from the rows {other:?}"
                    ));
                }
                if one.memory > 9 * G {
                    return Err(format!("{log}{name}: above the cap: {one:?}"));
                }
                let hold = holds.get(name).copied().unwrap_or_default();
                let expected = hold
                    .at
                    .is_some_and(|at| hold.clean < CLEAN_RUNS && now < at + UNPREDICTABLE_FOR);
                let unpredictable = matches!(one.basis, Basis::Unpredictable { .. });
                if expected != unpredictable {
                    return Err(format!(
                        "{log}{name}: held {expected} by its last cap event {hold:?}, predicted {one:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// With room for every job, one at a time: how many of a replay's runs peaked past what they reserved, against
    /// each reserving what the rows finished before it predict, read as the daemon reads them.
    fn replayed(seed: u64) -> Result<(), String> {
        // node at 9G now, its runs at 7G before.
        let config =
            Config::parse(&super::super::CONFIG.replace("memory = \"7G\"", "memory = \"9G\""))
                .unwrap();
        let mut rng = Rng(seed);
        let mut rows: Vec<Ran> = Vec::new();
        let mut under = 0;
        let mut at = NOW;
        for index in 0..RUNS {
            at += 2 * MINUTE + rng.below(3 * 24 * 60) * MINUTE;
            let Row { mut run, cancelled } = generate(&mut rng, index, at, None);
            if cancelled {
                continue;
            }
            if rng.below(8) == 0 {
                // Recorded before runners had caps: read under the profile's now.
                run.memory_cap = None;
            }
            let read = Predictor::read(
                &config.sizing,
                &History {
                    jobs: rows.clone(),
                    ..History::default()
                },
                |_| Some(9 * G),
            );
            let reserved = read
                .predict(&Key::of(&run), "node", 9 * G, at, Copies::Alone)
                .memory;
            // A run over today's cap is killed at it.
            under += u64::from(run.memory_peak.min(9 * G) > reserved);
            run.reserved = Some(reserved);
            rows.push(run);
        }
        let replay = simulate(
            &config,
            None,
            None,
            &rows,
            Policy::Measured,
            &std::collections::BTreeMap::from([("node".to_owned(), 2)]),
        );
        if replay.under_predicted == under {
            Ok(())
        } else {
            Err(format!(
                "{rows:#?}\n{} runs past their reservation, {under} by the rows before each",
                replay.under_predicted
            ))
        }
    }

    #[test]
    fn a_replay_reserves_each_start_what_the_rows_finished_before_it_predict() {
        for seed in 0..CASES {
            if let Err(failure) = replayed(seed) {
                panic!("seed {seed}:\n{failure}");
            }
        }
    }

    #[test]
    fn rows_learned_one_at_a_time_decide_as_the_rows_read_at_once_and_holds_end_by_clear_runs() {
        let cases = std::env::var("RUNNERD_INVARIANT_CASES")
            .ok()
            .and_then(|cases| cases.parse().ok())
            .unwrap_or(CASES);
        let seeds: Vec<u64> = match std::env::var("RUNNERD_INVARIANT_SEED") {
            Ok(seed) => vec![seed.parse().unwrap()],
            Err(_) => (0..cases).collect(),
        };
        for seed in seeds {
            if let Err(failure) = check(seed) {
                panic!("seed {seed}:\n{failure}");
            }
        }
    }
}
