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

/// `lowered` with a slot added an hour after the lowering, as auto sizing does when jobs waited: 10G × 4 since.
fn lowered_then_a_slot(after: Vec<Ran>) -> History {
    let mut history = lowered(after);
    let slot = LOWERED + HOUR;
    history.changes.push(SizingChange {
        at: slot / 1000,
        profile: "jvm".to_owned(),
        from: Resize {
            memory: 10 * G,
            slots: 3,
            cpus: None,
        },
        to: Resize {
            memory: 10 * G,
            slots: 4,
            cpus: None,
        },
        reason: "jobs waited".to_owned(),
        uncertain: false,
    });
    history.memory_set.push(set(10 * G, slot));
    history
}

#[test]
fn a_change_of_slots_after_a_lowering_does_not_keep_the_lowering_from_being_taken_back() {
    let text = jvm_at("10G", 4);
    let at = |hours: u64| ledger(NOW - 12 * HOUR + hours * HOUR, 10 * G, 6 * G);
    let cases = [
        ("a run that met 10G", vec![met(at(0))], RevertCause::Met),
        (
            "three failures on two branches",
            vec![
                failed(at(0), "main", 1),
                failed(at(1), "fix/parser", 2),
                failed(at(2), "main", 3),
            ],
            RevertCause::Failed { runs: 3 },
        ),
    ];
    for (case, runs, cause) in cases {
        let mut after = clear_at_ten(LOWERED + 2 * HOUR, 10);
        after.extend(runs);
        let history = lowered_then_a_slot(after);
        let changed = look(&text, &history, NOW, Some((LOWERED + HOUR) / 1000));
        assert_eq!(
            changed
                .iter()
                .map(|change| (change.to.memory, change.to.slots))
                .collect::<Vec<_>>(),
            [(14 * G, 2)],
            "{case}: back to the 14G × 2 the lowering came from"
        );
        let sizing = sizing(&text, &history);
        let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
        assert_eq!(
            jvm.revert.as_ref().map(|revert| revert.cause),
            Some(cause),
            "{case}"
        );
    }
}

#[test]
fn a_kill_at_a_hand_set_cap_raises_it_at_once_to_what_the_job_needed_at_larger_caps_however_far() {
    // Runs clear at 14G peaking 10.5G: 12.25G with the margin, more than one 25% step above 8G.
    let mut jobs: Vec<Ran> = (0..12)
        .map(|i| ledger(NOW - 20 * DAY + i * HOUR, 14 * G, 10752 * M))
        .collect();
    jobs.extend((0..10).map(|i| ledger(NOW - DAY + i * HOUR, 8 * G, 5 * G)));
    jobs.push(met(ledger(NOW - HOUR, 8 * G, 0)));
    let history = History {
        jobs,
        memory_set: vec![set(14 * G, NOW - 21 * DAY), set(8 * G, NOW - 2 * DAY)],
        ..History::default()
    };
    let changed = look(&jvm_at("8G", 2), &history, NOW, None);
    assert_eq!(
        changed
            .iter()
            .map(|change| (change.from.memory, change.to.memory))
            .collect::<Vec<_>>(),
        [(8 * G, 12544 * M)],
        "one raise, not a kill at each step"
    );
}

#[test]
fn a_revert_outside_the_profiles_bounds_raises_as_far_as_they_allow_and_the_profile_is_not_held() {
    let mut after = clear_at_ten(LOWERED + HOUR, 10);
    after.push(met(ledger(NOW - 2 * HOUR, 10 * G, 0)));
    // The owner put max_memory under the 14G the lowering came from.
    let text = jvm_at("10G", 3).replacen(
        "memory = \"10G\"",
        "memory = \"10G\"\nmax_memory = \"12G\"",
        1,
    );
    let changed = look(&text, &lowered(after), NOW, Some(LOWERED / 1000));
    let [change] = &changed[..] else {
        panic!("one change: {changed:?}")
    };
    assert_eq!(change.to.memory, 12 * G);
    assert!(
        change.reason.starts_with("raise memory to 12G")
            && change
                .reason
                .ends_with("as far as its max_memory allows, short of 14G"),
        "{}",
        change.reason
    );
}

#[test]
fn a_kill_in_the_last_sample_interval_near_the_cap_met_it_and_one_far_under_it_did_not() {
    // The container's own limit killed it before a sample counted the `oom`; Docker's flag still says it was killed.
    let killed = |peak: u64| Ran {
        oom_killed: true,
        hit_cap: Some(false),
        own_cap: Some(false),
        succeeded: false,
        failed: true,
        ..ledger(NOW - 2 * HOUR, 10 * G, peak)
    };
    let text = jvm_at("10G", 3);
    for (peak, reverted) in [(9900 * M, Some(RevertCause::Met)), (4 * G, None)] {
        let mut after = clear_at_ten(LOWERED + HOUR, 10);
        after.push(killed(peak));
        let sizing = sizing(&text, &lowered(after));
        let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
        assert_eq!(
            jvm.revert.as_ref().map(|revert| revert.cause),
            reverted,
            "killed with its last sample at {}M of 10G",
            peak / M
        );
        assert_eq!(jvm.peaks.on_cap, 1, "either kill holds its job");
    }
}

mod looks;
