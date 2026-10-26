//! A profile's `memory` by its jobs' recent runs (ADR 0070): raises only from runs that met the current cap since
//! the profile took it, a lowering that went too far reverted at the next look, and over generated days of a JVM
//! whose heap fills the room it has, lowerings that converge with at most one run meeting each.

use std::collections::BTreeMap;

use super::{CONFIG, G, HOUR, M, NOW, job};
use crate::api::{Advice, Resize, RevertCause, Sizing, SizingChange, SizingRevert, job_title};
use crate::config::Config;
use crate::sizing::auto::{Context, changes};
use crate::sizing::{History, MemorySet, Ran, Window, advise};

const DAY: u64 = 24 * HOUR;

/// `CONFIG` with jvm at `memory` × `slots` and auto sizing on.
fn jvm_at(memory: &str, slots: u32) -> String {
    CONFIG.replacen(
        "memory = \"9G\"\nslots = 2",
        &format!("memory = \"{memory}\"\nslots = {slots}"),
        1,
    ) + "\n[sizing]\nauto = true\n"
}

/// A run of halyard/ledger's `test` in jvm at `cap`, queued at `at`, peaking at `peak`, clear of its cap.
fn ledger(at: u64, cap: u64, peak: u64) -> Ran {
    Ran {
        memory_cap: Some(cap),
        queued_at: at,
        started_at: Some(at),
        runner_started_at: at,
        finished_at: Some(at + HOUR / 2),
        own_cap: Some(false),
        hit_cap: Some(false),
        run_id: at,
        ..job("halyard/ledger", "test", "jvm", peak, 0)
    }
}

/// `run` killed at its own cap, its last sample just under it.
fn met(run: Ran) -> Ran {
    let cap = run.memory_cap.unwrap_or(0);
    Ran {
        memory_peak: cap - 64 * M,
        oom_killed: true,
        hit_cap: Some(true),
        own_cap: Some(true),
        succeeded: false,
        failed: true,
        ..run
    }
}

/// `run` failed on `branch` in workflow run `run_id`, with no kernel event: a heap thrashing in GC.
fn failed(run: Ran, branch: &str, run_id: u64) -> Ran {
    Ran {
        succeeded: false,
        failed: true,
        branch: Some(branch.to_owned()),
        run_id,
        ..run
    }
}

fn set(memory: u64, at: u64) -> MemorySet {
    MemorySet {
        profile: "jvm".to_owned(),
        memory,
        at,
    }
}

fn window(now: u64) -> Window {
    Window {
        since: now - 30 * DAY,
        now,
        sample_interval: 2,
    }
}

fn sizing(text: &str, history: &History) -> Sizing {
    advise(
        &Config::parse(text).unwrap(),
        Some(64 * G),
        history,
        window(NOW),
    )
}

/// The changes due at `now` after jvm's last change `last` (seconds), nothing running.
fn look(text: &str, history: &History, now: u64, last: Option<u64>) -> Vec<SizingChange> {
    let last: BTreeMap<String, u64> = last.map(|at| ("jvm".to_owned(), at)).into_iter().collect();
    changes(
        &Config::parse(text).unwrap(),
        history,
        Context {
            memory_max: Some(64 * G),
            cpus: Some(32_000),
            window: window(now),
            last: &last,
            cpus_set: &[],
            running: &BTreeMap::new(),
        },
    )
}

/// The ledger's test as sizing names it.
fn ledger_test() -> String {
    format!("halyard/ledger {}", job_title(Some("CI"), "test", None))
}

/// When auto sizing lowered jvm from 14G × 2 to 10G × 3.
const LOWERED: u64 = NOW - 10 * DAY;

/// jvm at 14G for 10 days of clear 8.8G test runs, then lowered by auto sizing to 10G × 3, which the daemon ran
/// since; `after` are the runs at 10G.
fn lowered(after: Vec<Ran>) -> History {
    let mut jobs: Vec<Ran> = (0..12)
        .map(|i| ledger(NOW - 20 * DAY + i * HOUR, 14 * G, 8800 * M))
        .collect();
    jobs.extend(after);
    History {
        jobs,
        changes: vec![SizingChange {
            at: LOWERED / 1000,
            profile: "jvm".to_owned(),
            from: Resize {
                memory: 14 * G,
                slots: 2,
                cpus: None,
            },
            to: Resize {
                memory: 10 * G,
                slots: 3,
                cpus: None,
            },
            reason: "lower memory to 10G from 14G".to_owned(),
            uncertain: false,
        }],
        memory_set: vec![set(14 * G, NOW - 21 * DAY), set(10 * G, LOWERED)],
        ..History::default()
    }
}

/// `count` clear 6G test runs at 10G an hour apart from `start`.
fn clear_at_ten(start: u64, count: u64) -> Vec<Ran> {
    (0..count)
        .map(|i| ledger(start + i * HOUR, 10 * G, 6 * G))
        .collect()
}

#[test]
fn a_run_that_meets_a_lowered_cap_takes_the_profile_back_at_the_next_look_with_its_slots() {
    let mut after = clear_at_ten(LOWERED + HOUR, 10);
    after.push(met(ledger(NOW - 2 * HOUR, 10 * G, 0)));
    let history = lowered(after);
    let text = jvm_at("10G", 3);
    let sizing = sizing(&text, &history);
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert_eq!(
        jvm.advice,
        Advice::Raise {
            memory: 14 * G,
            slots: 2
        }
    );
    assert_eq!(
        jvm.revert,
        Some(SizingRevert {
            from: 10 * G,
            to: 14 * G,
            job: ledger_test(),
            cause: RevertCause::Met,
        })
    );
    // Ten minutes after a change of slots: a raise would wait for raise_interval, a revert does not.
    let changed = look(&text, &history, NOW, Some(NOW / 1000 - 600));
    assert_eq!(
        changed
            .iter()
            .map(|change| (change.profile.as_str(), change.to))
            .collect::<Vec<_>>(),
        vec![(
            "jvm",
            Resize {
                memory: 14 * G,
                slots: 2,
                cpus: None
            }
        )]
    );
    assert_eq!(changed[0].reason, sizing.memory_line(jvm));
    assert_eq!(
        changed[0].reason,
        format!(
            "raise memory back to 14G: {} met the 10G cap since auto sizing lowered it from 14G",
            ledger_test()
        )
    );
}

/// How a run at the lowered cap ended.
#[derive(Clone, Copy)]
enum Ended {
    Failed,
    Succeeded,
    Lost,
    Cancelled,
}

/// One run of a case: the job, its branch, its workflow run and how it ended.
type Step = (&'static str, &'static str, u64, Ended);

#[test]
fn three_failures_in_a_row_on_two_branches_revert_a_lowering_but_one_broken_branch_or_reruns_do_not()
 {
    use Ended::{Cancelled, Failed, Lost, Succeeded};
    let cases: [(&str, &[Step], Option<u32>); 7] = [
        (
            "three workflow runs on two branches",
            &[
                ("test", "main", 1, Failed),
                ("test", "fix/parser", 2, Failed),
                ("test", "main", 3, Failed),
            ],
            Some(3),
        ),
        (
            "one pull request pushed three times",
            &[
                ("test", "fix/parser", 1, Failed),
                ("test", "fix/parser", 2, Failed),
                ("test", "fix/parser", 3, Failed),
            ],
            None,
        ),
        (
            "a run's failed jobs re-run twice",
            &[
                ("test", "main", 1, Failed),
                ("test", "main", 1, Failed),
                ("test", "main", 1, Failed),
                ("test", "fix/parser", 2, Failed),
            ],
            None,
        ),
        (
            "a success between",
            &[
                ("test", "main", 1, Failed),
                ("test", "fix/parser", 2, Failed),
                ("test", "main", 3, Succeeded),
                ("test", "main", 4, Failed),
            ],
            None,
        ),
        (
            "lost runs",
            &[
                ("test", "main", 1, Lost),
                ("test", "fix/parser", 2, Lost),
                ("test", "main", 3, Lost),
            ],
            None,
        ),
        (
            "cancelled runs",
            &[
                ("test", "main", 1, Cancelled),
                ("test", "fix/parser", 2, Cancelled),
                ("test", "main", 3, Cancelled),
            ],
            None,
        ),
        (
            "a job that never succeeded at 14G",
            &[
                ("lint", "main", 1, Failed),
                ("lint", "fix/parser", 2, Failed),
                ("lint", "main", 3, Failed),
            ],
            None,
        ),
    ];
    let text = jvm_at("10G", 3);
    for (case, steps, reverted) in cases {
        let mut jobs = clear_at_ten(LOWERED + HOUR, 10);
        let mut cancelled = Vec::new();
        for (hours, &(name, branch, run_id, ended)) in (0..).zip(steps) {
            let run = Ran {
                name: name.to_owned(),
                ..ledger(NOW - 12 * HOUR + hours * HOUR, 10 * G, 6 * G)
            };
            match ended {
                Failed => jobs.push(failed(run, branch, run_id)),
                Succeeded => jobs.push(run),
                Lost => jobs.push(Ran {
                    failed: false,
                    ..failed(run, branch, run_id)
                }),
                Cancelled => cancelled.push(failed(run, branch, run_id)),
            }
        }
        let history = History {
            cancelled,
            ..lowered(jobs)
        };
        let sizing = sizing(&text, &history);
        let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
        assert_eq!(
            jvm.revert.as_ref().map(|revert| revert.cause),
            reverted.map(|runs| RevertCause::Failed { runs }),
            "{case}"
        );
    }
}

/// jvm at 14G for 10 days of clear 8.8G test runs (each needing 10G), then set to 8G by hand two days ago, which
/// the daemon ran since; `after` are the runs at 8G.
fn hand_set(after: Vec<Ran>) -> History {
    let mut jobs: Vec<Ran> = (0..12)
        .map(|i| ledger(NOW - 20 * DAY + i * HOUR, 14 * G, 8800 * M))
        .collect();
    jobs.extend(after);
    History {
        jobs,
        memory_set: vec![set(14 * G, NOW - 21 * DAY), set(8 * G, NOW - 2 * DAY)],
        ..History::default()
    }
}

#[test]
fn a_hand_set_cap_under_the_peaks_of_larger_caps_is_kept_until_a_run_meets_it() {
    let clear: Vec<Ran> = (0..10)
        .map(|i| ledger(NOW - DAY + i * HOUR, 8 * G, 5 * G))
        .collect();
    let text = jvm_at("8G", 4);
    let sizing = sizing(&text, &hand_set(clear));
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    // 8.8G + 15%, rounded up to 10G: what the runs at 14G need, but none met 8G.
    assert_eq!(jvm.advice, Advice::Keep { memory: 10 * G });
    assert_eq!(
        sizing.memory_line(jvm),
        "keep 8G: its recent runs need 10G, but none is known to have run out of memory at 8G since it was set"
    );
}

#[test]
fn a_hand_set_cap_a_run_meets_is_raised_toward_what_the_job_needed_at_larger_caps() {
    let mut runs: Vec<Ran> = (0..10)
        .map(|i| ledger(NOW - DAY + i * HOUR, 8 * G, 5 * G))
        .collect();
    runs.push(met(ledger(NOW - HOUR, 8 * G, 0)));
    let text = jvm_at("8G", 2);
    let sizing = sizing(&text, &hand_set(runs));
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    // Its last sample at the 8G cap says 9.25G; its runs at 14G say 10G.
    assert_eq!(
        (jvm.advice, jvm.revert.is_none()),
        (
            Advice::Raise {
                memory: 10 * G,
                slots: 2
            },
            true
        ),
        "a hand-set cap is raised, not reverted"
    );
}

#[test]
fn runs_that_met_the_cap_before_the_profile_took_it_again_raise_nothing() {
    // Killed at 9G three weeks ago, raised to 12G where they ran clear, then set back to 9G by hand yesterday.
    let mut jobs: Vec<Ran> = (0..10)
        .map(|i| met(ledger(NOW - 19 * DAY + i * HOUR, 9 * G, 0)))
        .collect();
    jobs.extend((0..3).map(|i| ledger(NOW - 10 * DAY + i * HOUR, 12 * G, 7 * G)));
    let history = History {
        jobs,
        memory_set: vec![
            set(9 * G, NOW - 20 * DAY),
            set(12 * G, NOW - 15 * DAY),
            set(9 * G, NOW - DAY),
        ],
        ..History::default()
    };
    let sizing = sizing(&jvm_at("9G", 2), &history);
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert!(
        matches!(jvm.advice, Advice::Keep { .. }),
        "kept, not raised: {:?}",
        jvm.advice
    );
}

#[test]
fn a_peak_near_the_cap_with_no_oom_at_it_puts_no_job_on_its_cap() {
    // A JVM filled 8.8G of a 9G cap and was never short of memory: its runs say it needs the cap it has, no more.
    let jobs = (0..12)
        .map(|i| ledger(NOW - DAY + i * HOUR, 9 * G, 8800 * M))
        .collect();
    let sizing = sizing(
        &jvm_at("9G", 2),
        &History {
            jobs,
            ..History::default()
        },
    );
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert_eq!(
        (jvm.peaks.at_cap, jvm.peaks.on_cap, jvm.advice),
        (0, 0, Advice::Keep { memory: 9 * G })
    );
}

#[test]
fn a_kill_recorded_before_the_local_counter_holds_the_cap_until_three_runs_ran_clear_and_raises_nothing()
 {
    let mut jobs: Vec<Ran> = (0..12)
        .map(|i| ledger(NOW - 2 * DAY + i * HOUR, 9 * G, 3 * G))
        .collect();
    jobs.push(Ran {
        own_cap: None,
        hit_cap: None,
        ..met(ledger(NOW - DAY, 9 * G, 0))
    });
    let text = jvm_at("9G", 2);
    let held = sizing(
        &text,
        &History {
            jobs: jobs.clone(),
            memory_set: vec![set(9 * G, NOW - 3 * DAY)],
            ..History::default()
        },
    );
    let jvm = held.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert_eq!(
        held.memory_line(jvm),
        "keep 9G: 1 of its jobs met their cap or were killed for memory, and fewer than 3 runs since ran clear of it"
    );
    jobs.extend((1..=3).map(|i| ledger(NOW - DAY + i * HOUR, 9 * G, 3 * G)));
    let released = sizing(
        &text,
        &History {
            jobs,
            memory_set: vec![set(9 * G, NOW - 3 * DAY)],
            ..History::default()
        },
    );
    let jvm = released.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert!(
        matches!(jvm.advice, Advice::Keep { .. } | Advice::Lower { .. }),
        "the old kill raises nothing: {:?}",
        jvm.advice
    );
    assert_eq!(jvm.peaks.on_cap, 0, "released by 3 clear runs");
}

#[test]
fn a_narrower_window_advises_memory_as_the_default_one_does() {
    let mut runs = clear_at_ten(LOWERED + HOUR, 30);
    runs.push(ledger(NOW - 5 * DAY, 10 * G, 8 * G));
    let history = lowered(runs);
    let config = Config::parse(&jvm_at("10G", 3)).unwrap();
    let two_days = advise(
        &config,
        Some(64 * G),
        &history,
        Window {
            since: NOW - 2 * DAY,
            ..window(NOW)
        },
    );
    let month = advise(&config, Some(64 * G), &history, window(NOW));
    for (narrow, wide) in two_days.profiles.iter().zip(&month.profiles) {
        assert_eq!(
            (narrow.advice, &narrow.peaks, &narrow.revert),
            (wide.advice, &wide.peaks, &wide.revert),
            "{}",
            narrow.name
        );
    }
}

mod days {
    //! Generated days of auto sizing: the ledger's tests in a JVM whose heap grows toward 63% of its cap, needing
    //! 5G, and 7G when a cold cache makes one run rebuild everything.

    use std::collections::BTreeMap;

    use super::{DAY, G, HOUR, M, NOW, jvm_at, ledger, look, met, set};
    use crate::api::SizingChange;
    use crate::config::{Config, resize_profile};
    use crate::sizing::History;
    use crate::sizing::memory::{CHANGES_READ, HISTORY};

    /// The look every 6 hours, with 3 runs before each.
    const LOOK: u64 = 6 * HOUR;

    /// The run at `cap` of a job that needs `need`: its heap fills 63% of the cap, and it meets a cap under `need`.
    fn ran(at: u64, cap: u64, need: u64, run_id: u64) -> crate::sizing::Ran {
        let run = ledger(at, cap, need.max(cap / 100 * 63));
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
            ..run
        };
        if need > cap { met(run) } else { run }
    }

    /// Days of runs and looks, with a cold cache once every `cold` runs, from 14G × 2; each lowering applied as
    /// the daemon applies it: written, recorded, and the profile's `memory` set from then.
    fn generate(days: u64, cold: u64) -> (Vec<(u64, u64)>, Vec<SizingChange>) {
        let mut text = jvm_at("14G", 2);
        let mut history = History {
            memory_set: vec![set(14 * G, NOW)],
            ..History::default()
        };
        let mut last = None;
        let mut met_at = Vec::new();
        let mut all = Vec::new();
        let mut run_id = 0;
        let mut now = NOW;
        while now < NOW + days * DAY {
            let cap = Config::parse(&text).unwrap().profiles["jvm"].memory.0;
            for i in 0..3 {
                run_id += 1;
                let need = if run_id % cold == 0 { 7 * G } else { 5 * G };
                let run = ran(now + i * HOUR, cap, need, run_id);
                if run.own_cap == Some(true) {
                    met_at.push((now + i * HOUR, cap));
                }
                history.jobs.push(run);
            }
            now += LOOK;
            // What the daemon reads: 30 days of runs, 60 of changes.
            history.jobs.retain(|run| run.queued_at >= now - HISTORY);
            history
                .changes
                .retain(|change| change.at * 1000 >= now - CHANGES_READ);
            for change in look(&text, &history, now, last) {
                text = resize_profile(&text, &change.profile, change.to).unwrap();
                history.memory_set.push(set(change.to.memory, now));
                history.changes.push(change.clone());
                all.push(change);
                last = Some(now / 1000);
            }
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
                        change.reason.starts_with("raise memory back to"),
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
