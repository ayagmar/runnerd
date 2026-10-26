//! Auto sizing's `cpus`: raised for a job its cap holds back over the whole run, not for a build whose bursts reach
//! any cap; lowered toward twice the heaviest job's average; and its invariants over generated sequences of runs,
//! looks, hand edits, undos and restarts.

use std::collections::BTreeMap;

use super::{CONFIG, G, HOUR, NOW, job};
use crate::api::{Resize, SizingChange};
use crate::config::{Admission, Config, cpu_budget, resize_profile};
use crate::sizing::auto::{Context, Look, next_look};
use crate::sizing::{History, Ran, Window, spread};

const DAY: u64 = 24 * HOUR;

/// `text` with `[sizing] auto` on and `rules` added.
fn on(text: &str, rules: &str) -> Config {
    Config::parse(&format!("{text}\n[sizing]\nauto = true\n{rules}")).unwrap()
}

/// `CONFIG` with jvm at `cpus`.
fn jvm_at(cpus: u32, rules: &str) -> Config {
    on(
        &CONFIG.replacen("cpus = 8", &format!("cpus = {cpus}"), 1),
        rules,
    )
}

/// `CONFIG` (jvm and node 2 × 8 CPUs) under a CPU budget of the slice's CPUs.
fn budgeted(rules: &str) -> Config {
    on(
        &CONFIG.replace(
            "isolation = \"sysbox\"",
            "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
        ),
        rules,
    )
}

/// `count` runs of halyard/ledger's `name` in jvm at `cpu_cap` millicores, queued at `at`, each averaging `average`
/// and, as a parallel build does, peaking at the cap.
fn runs(name: &str, count: usize, average: u64, cpu_cap: u64, at: u64) -> Vec<Ran> {
    (0..count)
        .map(|_| Ran {
            cpu_peak: cpu_cap,
            cpu_average: Some(average),
            cpu_cap: Some(cpu_cap),
            queued_at: at,
            started_at: Some(at + 5000),
            runner_started_at: at,
            finished_at: Some(at + HOUR / 2),
            ..job("halyard/ledger", name, "jvm", 7 * G, 5)
        })
        .collect()
}

fn history(jobs: Vec<Ran>) -> History {
    History {
        jobs,
        cancelled: Vec::new(),
        runners: Vec::new(),
    }
}

/// The look at `now` (ms) in a 33G slice of `cpus` millicores, after the last changes `last`, nothing running.
fn look_at(
    config: &Config,
    history: &History,
    cpus: Option<u64>,
    now: u64,
    last: &BTreeMap<String, u64>,
) -> Look {
    next_look(
        config,
        history,
        Context {
            memory_max: Some(33 * G),
            cpus,
            window: Window {
                since: now - 30 * DAY,
                now,
                sample_interval: 2,
            },
            last,
            running: &BTreeMap::new(),
        },
    )
}

fn look(config: &Config, history: &History, cpus: Option<u64>) -> Look {
    look_at(config, history, cpus, NOW, &BTreeMap::new())
}

/// Each change's profile and `cpus` from and to.
fn cpus_to(look: &Look) -> Vec<(&str, Option<u32>, Option<u32>)> {
    look.changes
        .iter()
        .map(|change| (change.profile.as_str(), change.from.cpus, change.to.cpus))
        .collect()
}

#[test]
fn a_parallel_build_that_peaks_at_any_cap_is_not_raised_while_its_runs_average_far_under_it() {
    // Like the Android quality gate on the owner's host: its bursts reach 8, 16 or 31 CPUs, whatever the cap, while
    // its runs average 6 of them. The old rule raised it on its peaks, 16 → 20 → 25 → 31 in a day.
    for cpus in [8, 16, 31] {
        let config = jvm_at(cpus, "");
        let built = history(runs(
            "android",
            3,
            5900.min(u64::from(cpus) * 650),
            u64::from(cpus) * 1000,
            NOW - HOUR,
        ));
        let look = look(&config, &built, Some(32_000));
        assert_eq!(look.changes, Vec::new(), "at {cpus} CPUs");
        assert_eq!(
            look.refused,
            Vec::new(),
            "nothing asks for a raise at {cpus}"
        );
    }
}

#[test]
fn a_job_whose_runs_average_at_the_cap_gets_more_cpus_up_to_the_slices() {
    let config = jvm_at(8, "");
    let pinned = history(runs("test", 3, 7900, 8000, NOW - HOUR));
    let raised = look(&config, &pinned, Some(32_000));
    assert_eq!(
        cpus_to(&raised),
        vec![("jvm", Some(8), Some(10))],
        "8 CPUs + 25%"
    );
    assert_eq!(
        (raised.changes[0].to.memory, raised.changes[0].to.slots),
        (9 * G, 2),
        "memory and slots stay"
    );
    assert!(
        raised.changes[0]
            .reason
            .contains("3 runs of halyard/ledger CI / test averaged 7.9 of its 8 CPUs (median)"),
        "{}",
        raised.changes[0].reason
    );
    assert_eq!(
        cpus_to(&look(&config, &pinned, Some(9_000))),
        vec![("jvm", Some(8), Some(9))],
        "at most the slice's CPUs"
    );
    let unknown = look(&config, &pinned, None);
    assert_eq!(unknown.changes, Vec::new(), "not without the slice's CPUs");
    assert!(
        unknown.refused[0]
            .reason
            .ends_with("the slice's CPUs could not be read"),
        "{:?}",
        unknown.refused
    );
    let full = look(&config, &pinned, Some(8_000));
    assert_eq!(full.changes, Vec::new());
    assert!(
        full.refused[0]
            .reason
            .ends_with("it has all 8 of the slice's CPUs"),
        "{:?}",
        full.refused
    );
    for (other, why) in [
        (
            runs("test", 3, 5900, 6000, NOW - HOUR),
            "runs at another cap",
        ),
        (runs("test", 2, 7900, 8000, NOW - HOUR), "two runs"),
    ] {
        assert_eq!(
            look(&config, &history(other), Some(32_000)).changes,
            Vec::new(),
            "{why} say nothing about this cap"
        );
    }
}

#[test]
fn one_job_held_back_by_the_cap_raises_it_however_light_the_profiles_other_jobs() {
    // Three image builds at 6 of 8 CPUs (75%) beside twenty lint runs at a tenth of a CPU: the profile's mean is far
    // under the cap, the build is not.
    let config = jvm_at(8, "");
    let mut jobs = runs("lint", 20, 100, 8000, NOW - HOUR);
    jobs.extend(runs("image", 3, 6000, 8000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look(&config, &history(jobs), Some(32_000))),
        vec![("jvm", Some(8), Some(10))]
    );
}

#[test]
fn failed_cancelled_and_unmeasured_runs_raise_nothing() {
    let config = jvm_at(8, "");
    let pinned = runs("test", 3, 7900, 8000, NOW - HOUR);
    let failed: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            succeeded: false,
            ..run.clone()
        })
        .collect();
    let unmeasured: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            cpu_average: None,
            ..run.clone()
        })
        .collect();
    let sampled_once: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            cpu_average: Some(0),
            ..run.clone()
        })
        .collect();
    let uncapped: Vec<Ran> = pinned
        .iter()
        .map(|run| Ran {
            cpu_cap: None,
            ..run.clone()
        })
        .collect();
    for (jobs, why) in [
        (failed, "failed"),
        (unmeasured, "without an average"),
        (sampled_once, "sampled once"),
        (uncapped, "without a recorded cap"),
    ] {
        assert_eq!(
            look(&config, &history(jobs), Some(32_000)).changes,
            Vec::new(),
            "runs {why} say nothing"
        );
    }
    let cancelled = History {
        jobs: Vec::new(),
        cancelled: pinned,
        runners: Vec::new(),
    };
    assert_eq!(
        look(&config, &cancelled, Some(32_000)).changes,
        Vec::new(),
        "cancelled runs say nothing"
    );
}

#[test]
fn an_over_provisioned_profile_steps_down_to_its_target_over_several_intervals_and_stops_there() {
    // jvm at 31 CPUs, as auto sizing left it on the owner's host. Its heaviest job averages 5 to 7.75 CPUs, 7.75 at
    // p95; twice that is 15.5, rounded up to 16. Each day brings ten runs at the cap of that day, and a lint job
    // averaging half a CPU.
    let mut config = jvm_at(31, "");
    let mut text = CONFIG.replacen("cpus = 8", "cpus = 31", 1);
    let mut jobs = Vec::new();
    let mut last = BTreeMap::new();
    let mut seen = vec![31];
    for day in 0..6 {
        let now = NOW + day * DAY;
        let cap = u64::from(config.profiles["jvm"].cpus) * 1000;
        for i in 0..10 {
            jobs.extend(runs("android", 1, 5000 + 250 * (i + 2), cap, now - HOUR));
        }
        jobs.extend(runs("lint", 4, 500, cap, now - HOUR));
        let look = look_at(&config, &history(jobs.clone()), Some(32_000), now, &last);
        for change in &look.changes {
            text = resize_profile(&text, &change.profile, change.to).unwrap();
            last.insert(change.profile.clone(), change.at);
        }
        config = on(&text, "");
        seen.push(config.profiles["jvm"].cpus);
        if let Some(change) = look.changes.first() {
            assert!(
                change
                    .reason
                    .contains("halyard/ledger CI / android, averaged 7.8 CPUs (p95)"),
                "{}",
                change.reason
            );
        }
    }
    assert_eq!(
        seen,
        [31, 24, 18, 16, 16, 16, 16],
        "a step of at most 25% a day, down to 16"
    );
}

#[test]
fn a_lowering_waits_the_interval_and_min_jobs_runs_since_the_last_change() {
    let config = jvm_at(8, "");
    let idle = history(runs("test", 10, 1000, 8000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look(&config, &idle, Some(32_000))),
        vec![("jvm", Some(8), Some(6))],
        "twice 1 CPU is 2, but a step is at most 25%"
    );
    let changed = BTreeMap::from([("jvm".to_owned(), (NOW - 3 * HOUR) / 1000)]);
    let held = look_at(&config, &idle, Some(32_000), NOW, &changed);
    assert_eq!(held.changes, Vec::new(), "a lowering waits the interval");
    let before = BTreeMap::from([("jvm".to_owned(), (NOW - 25 * HOUR) / 1000)]);
    let mut old = runs("test", 9, 1000, 8000, NOW - 26 * HOUR);
    old.extend(runs("test", 9, 1000, 8000, NOW - HOUR));
    assert_eq!(
        look_at(&config, &history(old), Some(32_000), NOW, &before).changes,
        Vec::new(),
        "9 runs since the last change are fewer than min_jobs; older ones do not count"
    );
}

#[test]
fn a_cap_lowered_undone_or_edited_back_reads_only_the_runs_since() {
    let config = jvm_at(8, "");
    // Runs pinned at 8 CPUs, then a raise to 10, ten light runs there, and a lowering, undo or hand edit back to 8.
    let mut jobs = runs("test", 3, 7900, 8000, NOW - 80 * HOUR);
    jobs.extend(runs("test", 10, 2000, 10_000, NOW - 3 * HOUR));
    let raised = BTreeMap::from([("jvm".to_owned(), (NOW - 80 * HOUR + HOUR) / 1000)]);
    let lowered = BTreeMap::from([("jvm".to_owned(), (NOW - 2 * HOUR) / 1000)]);
    for (last, how) in [
        (&lowered, "a lowering"),
        (&raised, "an undo or a hand edit"),
    ] {
        assert_eq!(
            look_at(&config, &history(jobs.clone()), Some(32_000), NOW, last).changes,
            Vec::new(),
            "after {how}, the old runs at 8 say nothing new"
        );
    }
    jobs.extend(runs("test", 3, 7900, 8000, NOW - HOUR));
    assert_eq!(
        cpus_to(&look_at(
            &config,
            &history(jobs),
            Some(32_000),
            NOW,
            &raised
        )),
        vec![("jvm", Some(8), Some(10))],
        "runs at the cap since then raise it"
    );
}

#[test]
fn a_raise_that_does_not_fit_the_cpu_budget_is_not_written_and_says_why() {
    // jvm 2 × 8 and node 2 × 8 fill a 32-CPU budget: say node's memory lowering added slots once a lowering of its
    // cpus freed the budget. jvm's tests now sit at its cap: no CPU is left for them.
    let config = budgeted("");
    let pinned = history(runs("test", 3, 7900, 8000, NOW - HOUR));
    let full = look(&config, &pinned, Some(32_000));
    assert_eq!(full.changes, Vec::new());
    assert_eq!(full.refused.len(), 1);
    assert_eq!(full.refused[0].profile, "jvm");
    assert!(
        full.refused[0].reason.ends_with(
            "9 CPUs for each of its 2 slots do not fit the CPU budget beside the other profiles'"
        ),
        "{}",
        full.refused[0].reason
    );
    assert_eq!(
        cpus_to(&look(&config, &pinned, Some(36_000))),
        vec![("jvm", Some(8), Some(10))],
        "4 CPUs more hold 2 slots at 10"
    );
}

#[test]
fn a_slot_the_cpu_budget_refuses_does_not_also_refuse_a_cpu_raise_that_fits_without_it() {
    // jvm's three runs sat at its 8 CPUs and waited 5 minutes. A 36-CPU budget beside node's 16 holds 2 jvm slots at
    // 10 CPUs, not 3 at 8.
    let waited: Vec<Ran> = runs("test", 3, 7900, 8000, NOW - HOUR)
        .into_iter()
        .map(|run| Ran {
            started_at: Some(run.queued_at + 300_000),
            ..run
        })
        .collect();
    let raised = next_look(
        &budgeted(""),
        &history(waited),
        Context {
            memory_max: Some(64 * G),
            cpus: Some(36_000),
            window: Window {
                since: NOW - 30 * DAY,
                now: NOW,
                sample_interval: 2,
            },
            last: &BTreeMap::new(),
            running: &BTreeMap::new(),
        },
    );
    assert_eq!(
        raised
            .changes
            .iter()
            .map(|change| (change.profile.as_str(), change.to.slots, change.to.cpus))
            .collect::<Vec<_>>(),
        [("jvm", 2, Some(10))],
        "{raised:?}"
    );
}

#[test]
fn memory_comes_down_once_the_kill_and_the_large_peaks_leave_the_history() {
    // A run killed for memory at jvm's 9G cap 29 days ago, and ten runs since peaking at 3G. Sizing reads 30 days:
    // the kill holds the cap until it is older than that, then the lowering comes, a step at a time.
    let config = jvm_at(8, "");
    let killed = Ran {
        oom_killed: true,
        ..runs("test", 1, 4000, 8000, NOW - 29 * DAY).remove(0)
    };
    let light: Vec<Ran> = runs("test", 10, 4000, 8000, NOW - HOUR)
        .into_iter()
        .map(|run| Ran {
            memory_peak: 3 * G,
            ..run
        })
        .collect();
    let read = |now: u64| {
        let mut jobs = vec![killed.clone()];
        jobs.extend(light.iter().cloned());
        jobs.retain(|job| job.queued_at >= now - 30 * DAY);
        look_at(&config, &history(jobs), Some(32_000), now, &BTreeMap::new())
    };
    assert_eq!(read(NOW).changes, Vec::new(), "the kill holds the cap");
    let later = read(NOW + 2 * DAY);
    assert_eq!(
        later
            .changes
            .iter()
            .map(|change| (change.profile.as_str(), change.to.memory))
            .collect::<Vec<_>>(),
        [("jvm", 6912 * (1 << 20))],
        "9G less 25%, rounded up to 256M: {later:?}"
    );
}

/// The invariants of `cpus` over generated sequences. The sequences come from a seeded generator, so a failure
/// replays: it prints its seed and the events up to it. `RUNNERD_INVARIANT_CASES` runs more cases than the default,
/// `RUNNERD_INVARIANT_SEED` one seed.
mod invariants {
    use std::fmt::Write as _;

    use super::*;

    const CASES: u64 = 64;
    const EVENTS: usize = 80;
    const PROFILES: [&str; 2] = ["jvm", "node"];
    /// The jobs a profile runs, each with how heavy it tends to be, in percent of the cap.
    const JOBS: [(&str, u64); 4] = [("build", 35), ("lint", 5), ("image", 75), ("e2e", 95)];

    /// A deterministic generator (`SplitMix64`), as the engine's invariants use.
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

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[usize::try_from(self.below(items.len() as u64)).unwrap()]
        }
    }

    /// A host: its config file as auto sizing and hand edits leave it, the history the store would read, when auto
    /// sizing last changed each profile and what each change replaced.
    struct World {
        text: String,
        rules: String,
        slice: Option<u64>,
        jobs: Vec<Ran>,
        cancelled: Vec<Ran>,
        last: BTreeMap<String, u64>,
        undo: BTreeMap<String, Resize>,
        now: u64,
        log: String,
    }

    impl World {
        fn config(&self) -> Config {
            on(&self.text, &self.rules)
        }

        /// The last 30 days, as the store reads them.
        fn history(&self) -> History {
            let since = self.now - 30 * DAY;
            History {
                jobs: self
                    .jobs
                    .iter()
                    .filter(|job| job.queued_at >= since)
                    .cloned()
                    .collect(),
                cancelled: self
                    .cancelled
                    .iter()
                    .filter(|job| job.queued_at >= since)
                    .cloned()
                    .collect(),
                runners: Vec::new(),
            }
        }

        fn look(&self) -> Look {
            look_at(
                &self.config(),
                &self.history(),
                self.slice,
                self.now,
                &self.last,
            )
        }

        fn write(&mut self, profile: &str, to: Resize) {
            self.text = resize_profile(&self.text, profile, to).unwrap();
        }

        /// Runs of one of `profile`'s jobs at its cap now, some failed, cancelled or unmeasured.
        fn run(&mut self, rng: &mut Rng, profile: &str) {
            let config = self.config();
            let limits = &config.profiles[profile];
            let cap = u64::from(limits.cpus) * 1000;
            let (name, heavy) = rng.pick(&JOBS);
            for _ in 0..=rng.below(5) {
                let share = heavy.saturating_sub(10) + rng.below(21);
                let mut ran = Ran {
                    profile: profile.to_owned(),
                    memory_cap: Some(limits.memory.0),
                    memory_peak: limits.memory.0 / 4 + rng.below(limits.memory.0 / 2),
                    cpu_cap: Some(cap),
                    cpu_peak: cap,
                    cpu_average: Some((cap * share / 100).max(1)),
                    queued_at: self.now,
                    started_at: Some(self.now + rng.below(300) * 1000),
                    runner_started_at: self.now,
                    finished_at: Some(self.now + HOUR / 4),
                    ..job("halyard/ledger", name, profile, 0, 0)
                };
                match rng.below(20) {
                    0 | 1 => ran.succeeded = false,
                    2 => ran.cpu_average = None,
                    3 => ran.cpu_cap = None,
                    4 => ran.cpu_average = Some(0),
                    5 | 6 => {
                        self.cancelled.push(ran);
                        continue;
                    }
                    _ => {}
                }
                self.jobs.push(ran);
            }
            self.now += 1000 * rng.below(600);
        }
    }

    /// What `cpus` of `profile` at `cap` (ms) may move on: successful runs at the cap with a measured average, since
    /// its last change and its last run under another cap. Written apart from the rule it checks.
    fn evidence(world: &World, profile: &str, cap: u64) -> BTreeMap<&'static str, Vec<u64>> {
        let history = world.history();
        let since = world
            .last
            .get(profile)
            .map_or(0, |at| at * 1000)
            .max(world.now - 30 * DAY);
        let other = history
            .jobs
            .iter()
            .chain(&history.cancelled)
            .filter(|job| job.profile == profile && job.cpu_cap.is_some_and(|other| other != cap))
            .map(|job| job.queued_at + 1)
            .max()
            .unwrap_or(0);
        let mut jobs: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for job in history.jobs.iter().filter(|job| {
            job.profile == profile
                && job.cpu_cap == Some(cap)
                && job.succeeded
                && job.queued_at >= since.max(other)
                && job.cpu_average.is_some_and(|average| average > 0)
        }) {
            let name = JOBS.iter().find(|(name, _)| *name == job.name).unwrap().0;
            jobs.entry(name).or_default().push(job.cpu_average.unwrap());
        }
        jobs
    }

    /// The CPUs every profile's slots take, millicores.
    fn taken(config: &Config) -> u64 {
        config
            .profiles
            .values()
            .map(|profile| u64::from(profile.cpus) * 1000 * u64::from(profile.slots))
            .sum()
    }

    /// Checks a look's `cpus` changes against the evidence and the rules; `Err` says what broke.
    fn check(world: &World, look: &Look) -> Result<(), String> {
        let config = world.config();
        let rules = &config.sizing;
        for change in &look.changes {
            let SizingChange {
                profile, from, to, ..
            } = change;
            let (Some(from_cpus), Some(to_cpus)) = (from.cpus, to.cpus) else {
                continue;
            };
            let current = config.profiles[profile].cpus;
            if from_cpus != current {
                return Err(format!("{profile}: from {from_cpus} CPUs, has {current}"));
            }
            let cap = u64::from(current) * 1000;
            let jobs = evidence(world, profile, cap);
            let since = world
                .last
                .get(profile)
                .map_or(u64::MAX, |at| world.now / 1000 - at);
            let step = (current * rules.max_step / 100).max(1);
            if to_cpus == 0 || current.abs_diff(to_cpus) > step {
                return Err(format!(
                    "{profile}: {current} → {to_cpus} CPUs, more than a step"
                ));
            }
            if to_cpus > current {
                let bound = jobs.values().any(|averages| {
                    averages.len() >= 3 && spread(averages.clone()).p50 * 100 >= cap * 70
                });
                if !bound {
                    return Err(format!(
                        "{profile}: raised without a job at its cap: {jobs:?}"
                    ));
                }
                if world
                    .slice
                    .is_none_or(|slice| u64::from(to_cpus) * 1000 > slice)
                {
                    return Err(format!("{profile}: raised past the slice's CPUs"));
                }
                if since < rules.raise_after().as_secs() {
                    return Err(format!("{profile}: raised {since}s after its last change"));
                }
            } else {
                let runs: usize = jobs.values().map(Vec::len).sum();
                if runs < rules.min_jobs as usize {
                    return Err(format!("{profile}: lowered on {runs} runs"));
                }
                let heaviest = jobs
                    .values()
                    .map(|averages| spread(averages.clone()).p95)
                    .max()
                    .unwrap_or(u64::MAX);
                if u64::from(to_cpus) * 1000 < heaviest * 2 {
                    return Err(format!(
                        "{profile}: lowered to {to_cpus} CPUs under twice its heaviest job's {heaviest}"
                    ));
                }
                if since < rules.interval.as_secs() {
                    return Err(format!("{profile}: lowered {since}s after its last change"));
                }
            }
        }
        if let (Admission::Caps, Some(overcommit), Some(slice)) = (
            config.host.admission,
            config.host.cpu_overcommit,
            world.slice,
        ) {
            let mut after = world.text.clone();
            for change in &look.changes {
                after = resize_profile(&after, &change.profile, change.to).unwrap();
            }
            let (before, after) = (taken(&config), taken(&on(&after, &world.rules)));
            let budget = cpu_budget(slice, overcommit);
            if after > before.max(budget) {
                return Err(format!(
                    "slots × cpus {before} → {after} past the budget {budget}"
                ));
            }
        }
        Ok(())
    }

    fn case(seed: u64) -> Result<(), String> {
        let mut rng = Rng(seed);
        let budget = rng.below(2) == 0;
        let mut text = CONFIG.to_owned();
        if budget {
            text = text.replace(
                "isolation = \"sysbox\"",
                "isolation = \"sysbox\"\ncpu_overcommit = 1.0",
            );
        }
        let mut world = World {
            text,
            rules: rng
                .pick(&["", "min_jobs = 3", "max_step = 50\nmin_jobs = 5"])
                .to_owned(),
            slice: rng.pick(&[Some(32_000), Some(48_000), None]),
            jobs: Vec::new(),
            cancelled: Vec::new(),
            last: BTreeMap::new(),
            undo: BTreeMap::new(),
            now: NOW,
            log: String::new(),
        };
        let _ = writeln!(
            world.log,
            "budget {budget}, rules {:?}, slice {:?}",
            world.rules, world.slice
        );
        for _ in 0..EVENTS {
            match rng.below(10) {
                0..=3 => {
                    let profile = rng.pick(&PROFILES);
                    world.run(&mut rng, profile);
                    let _ = writeln!(world.log, "runs of {profile}");
                }
                4 | 5 => {
                    let hours = rng.pick(&[1, 2, 25, 72]);
                    world.now += hours * HOUR;
                    let _ = writeln!(world.log, "{hours}h pass");
                }
                6 | 7 => {
                    let look = world.look();
                    // Stateless beyond the history and the change records: a restart decides the same.
                    if world.look() != look {
                        return Err(format!("{}a restart decides otherwise", world.log));
                    }
                    check(&world, &look).map_err(|broke| format!("{}{broke}", world.log))?;
                    let config = world.config();
                    for change in look.changes {
                        let _ = writeln!(world.log, "look: {} {}", change.profile, change.line());
                        let profile = &config.profiles[&change.profile];
                        world.undo.insert(
                            change.profile.clone(),
                            Resize {
                                cpus: Some(profile.cpus),
                                ..change.from
                            },
                        );
                        world.write(&change.profile, change.to);
                        world.last.insert(change.profile.clone(), change.at);
                    }
                }
                8 => {
                    let profile = rng.pick(&PROFILES);
                    let config = world.config();
                    let limits = &config.profiles[profile];
                    let cpus = u32::try_from(1 + rng.below(32)).unwrap();
                    world.write(
                        profile,
                        Resize {
                            memory: limits.memory.0,
                            slots: limits.slots,
                            cpus: Some(cpus),
                        },
                    );
                    let _ = writeln!(world.log, "hand edit: {profile} cpus {cpus}");
                }
                _ => {
                    let profile = rng.pick(&PROFILES);
                    if let Some(from) = world.undo.remove(profile) {
                        world.write(profile, from);
                        let _ = writeln!(world.log, "undo {profile}: {from:?}");
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn cpus_move_only_on_fresh_evidence_within_the_budget_and_a_step_a_window() {
        let env = |name| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
        };
        let seeds: Vec<u64> = if let Some(seed) = env("RUNNERD_INVARIANT_SEED") {
            vec![seed]
        } else {
            (0..env("RUNNERD_INVARIANT_CASES").unwrap_or(CASES)).collect()
        };
        for seed in seeds {
            if let Err(broke) = case(seed) {
                panic!("seed {seed}:\n{broke}");
            }
        }
    }
}
