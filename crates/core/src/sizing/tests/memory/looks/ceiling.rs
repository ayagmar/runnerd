//! `[host] memory_ceiling` bounds what auto sizing writes as the config check bounds it (ADR 0086): a raise or a
//! revert goes no further than the ceiling leaves one runner beside the mirror, every config it writes passes the
//! check, and a look at the bound proposes nothing new until a reload moves the ceiling or the mirror.

use super::super::{LOWERED, clear_at_ten, failed, ledger_test, look, lowered, sizing};
use super::{CpusSet, Daemon, G, HOUR, NOW, SizingChange, jvm_at, ledger, met, window};
use crate::api::{Advice, Resize, Sizing};
use crate::config::{ByteSize, Config, resize_profile};
use crate::sizing::{History, advise};

impl Daemon {
    /// Started at `now` with `text` and `history` against a slice of `memory_max`, as after a restart.
    pub(super) fn restarted(text: String, history: History, memory_max: u64, now: u64) -> Daemon {
        let mut daemon = Daemon {
            text,
            history,
            cpus_set: Vec::<CpusSet>::new(),
            now,
            memory_max,
        };
        daemon.announce();
        daemon
    }

    /// The advice as `runnerctl sizing` shows it after the last look.
    pub(super) fn sizing(&self) -> Sizing {
        advise(
            &self.config(),
            Some(self.memory_max),
            &self.history,
            window(self.now),
        )
    }

    /// A look at `now`, whose config, if it wrote one, the config check passes.
    pub(super) fn checked_look(&mut self, now: u64) -> Vec<SizingChange> {
        let changes = self.look(now);
        if let Err(error) = check(&self.text, self.memory_max) {
            panic!("the look at {now} wrote a config the check refuses: {error}");
        }
        changes
    }
}

/// `text` with `line` added under `[host]`.
pub(super) fn host(text: &str, line: &str) -> String {
    text.replacen(
        "isolation = \"sysbox\"\n",
        &format!("isolation = \"sysbox\"\n{line}\n"),
        1,
    )
}

/// `text` with the mirror off.
fn mirror_off(text: &str) -> String {
    text.replacen("mirror = { enabled = true", "mirror = { enabled = false", 1)
}

/// Whether the config check against a slice of `memory_max` passes `text`, and why not.
pub(super) fn check(text: &str, memory_max: u64) -> Result<(), String> {
    Config::parse(text)
        .map_err(|error| error.to_string())?
        .validate(Some(ByteSize(memory_max)))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// jvm's memory changes among `changes`, as (from, to).
pub(super) fn memory_steps(changes: &[SizingChange]) -> Vec<(u64, u64)> {
    changes
        .iter()
        .filter(|change| change.profile == "jvm" && change.to.memory != change.from.memory)
        .map(|change| (change.from.memory, change.to.memory))
        .collect()
}

/// Every 2 hours from `start`, `count` times, a ledger test run that meets jvm's cap and a look; every config the
/// looks write passes the check.
fn meeting_the_cap(daemon: &mut Daemon, start: u64, count: u64) -> Vec<SizingChange> {
    let mut changes = Vec::new();
    for i in 0..count {
        let at = start + i * 2 * HOUR;
        let cap = daemon.cap();
        daemon.history.jobs.push(met(ledger(at, cap, 0)));
        changes.extend(daemon.checked_look(at + HOUR));
    }
    changes
}

#[test]
fn a_raise_stops_at_what_the_memory_ceiling_leaves_one_runner_and_later_looks_raise_no_further() {
    for admission in ["caps", "measured"] {
        for mirror in [true, false] {
            let case = format!("{admission}, mirror {}", if mirror { "on" } else { "off" });
            // Both leave one runner 11.9G, which a config writes as 11.75G, the granule under it.
            let (ceiling, text) = if mirror {
                ("12.4G", jvm_at("10G", 2))
            } else {
                ("11.9G", mirror_off(&jvm_at("10G", 2)))
            };
            let text = host(
                &text,
                &format!("admission = \"{admission}\"\nmemory_ceiling = \"{ceiling}\""),
            );
            let mut daemon = Daemon::new(text, NOW);
            // Enough clear runs at 10G for advice, then every run meets the cap.
            for i in 0..12 {
                let at = NOW - 30 * HOUR + i * 2 * HOUR;
                daemon.history.jobs.push(ledger(at, 10 * G, 6 * G));
            }
            let changes = meeting_the_cap(&mut daemon, NOW, 48);
            // 10G + 15% is 11.5G; 11.5G + 15% is 13.25G, which the ceiling holds at 11.75G.
            assert_eq!(
                memory_steps(&changes),
                [(10 * G, 11776 << 20), (11776 << 20, 12032 << 20)],
                "{case}: to the need, then to the bound, then nothing more"
            );
            let held = &changes
                .iter()
                .find(|change| change.to.memory == 12032 << 20)
                .unwrap()
                .reason;
            let beside = if mirror {
                " beside the mirror's 512M"
            } else {
                ""
            };
            assert!(
                held.starts_with(&format!(
                    "raise memory to 11.75G, the most the host's {ceiling} memory_ceiling leaves one \
                     runner{beside}, short of the 13.25G its jobs need: "
                )),
                "{case}: {held}"
            );
            let sizing = daemon.sizing();
            let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
            assert!(jvm.held().is_some(), "{case}: jvm is held at the ceiling");
            assert_eq!(jvm.advice_word(), "at ceiling", "{case}");
            assert!(
                sizing.advice_line(jvm).starts_with(&format!(
                    "keep 11.75G, the most the host's {ceiling} memory_ceiling leaves one runner{beside}: its \
                     jobs need 13.75G"
                )),
                "{case}: {}",
                sizing.advice_line(jvm)
            );
            // The job's own advice still says what it needs.
            assert_eq!(jvm.jobs[0].needs().text, "13.75G > cap", "{case}");

            // A reload that frees room past the bound lets the next look raise to it, and no further.
            let freed = if mirror {
                mirror_off(&daemon.text)
            } else {
                daemon.text.replace(ceiling, "24G")
            };
            daemon.text = freed;
            daemon.announce();
            let at = NOW + 100 * HOUR;
            let raised = meeting_the_cap(&mut daemon, at, 1);
            let to = if mirror { 12544 << 20 } else { 14080 << 20 };
            assert_eq!(
                memory_steps(&raised),
                [(12032 << 20, to)],
                "{case}: the reload's bound applies at the next look"
            );
        }
    }
}

#[test]
fn a_revert_past_a_ceiling_lowered_since_goes_back_as_far_as_the_ceiling_leaves() {
    // jvm was lowered from 14G × 2 to 10G × 3, then a reload set a ceiling that leaves one runner 11.75G.
    let mut after = clear_at_ten(LOWERED + HOUR, 10);
    after.push(met(ledger(NOW - 2 * HOUR, 10 * G, 0)));
    let history = lowered(after);
    let text = host(&jvm_at("10G", 3), "memory_ceiling = \"12.4G\"");
    // Ten minutes after a change: a revert does not wait for raise_interval, bounded or not.
    let changed = look(&text, &history, NOW, Some(NOW / 1000 - 600));
    assert_eq!(
        changed.iter().map(|change| change.to).collect::<Vec<_>>(),
        [Resize {
            memory: 12032 << 20,
            slots: 2,
            cpus: None
        }]
    );
    assert_eq!(
        changed[0].reason,
        format!(
            "raise memory back to 11.75G, the most the host's 12.4G memory_ceiling leaves one runner beside the \
             mirror's 512M: {} met the 10G cap since auto sizing lowered it from 14G",
            ledger_test()
        )
    );
    let written = resize_profile(&text, "jvm", changed[0].to).unwrap();
    assert_eq!(
        check(&written, 64 * G),
        Ok(()),
        "the revert passes the check"
    );
}

#[test]
fn a_revert_the_ceiling_leaves_no_room_for_changes_nothing_and_keeps_its_evidence() {
    // The ceiling leaves one runner exactly the 10G jvm was lowered to.
    let mut after = clear_at_ten(LOWERED + HOUR, 10);
    after.push(met(ledger(NOW - 2 * HOUR, 10 * G, 0)));
    let history = lowered(after);
    let text = host(&jvm_at("10G", 3), "memory_ceiling = \"10.5G\"");
    assert_eq!(
        check(&text, 64 * G),
        Ok(()),
        "10G and the mirror fill the ceiling"
    );
    assert_eq!(look(&text, &history, NOW, Some(NOW / 1000 - 600)), []);
    let sizing = sizing(&text, &history);
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert_eq!(jvm.advice, Advice::SliceTooSmall { memory: 14 * G });
    assert!(jvm.revert.is_some(), "the lowering still went too far");
    assert_eq!(
        sizing.advice_line(jvm),
        format!(
            "keep 10G, the most the host's 10.5G memory_ceiling leaves one runner beside the mirror's 512M: its \
             jobs need 14G ({} met the 10G cap since auto sizing lowered it from 14G); raise [host] \
             memory_ceiling if the host can grow further",
            ledger_test()
        )
    );
    let fix = sizing.doctor_check().fix.unwrap();
    assert!(
        fix.contains("memory_ceiling") && !fix.contains("MemoryMax"),
        "the fix grows the ceiling, not the slice: {fix}"
    );
}

#[test]
fn a_raise_at_terabyte_scale_writes_the_bound_to_the_byte_and_stays_there() {
    // 1.006T less the mirror leaves one runner 1029.64G, 1029.5G down to a granule, which `Display` says as 1.01T.
    let text = host(&jvm_at("1000G", 1), "memory_ceiling = \"1.006T\"");
    let mut daemon = Daemon::restarted(text, History::default(), 4 << 40, NOW);
    for i in 0..12 {
        let at = NOW - 30 * HOUR + i * 2 * HOUR;
        daemon.history.jobs.push(ledger(at, 1000 * G, 600 * G));
    }
    let changes = meeting_the_cap(&mut daemon, NOW, 24);
    assert_eq!(memory_steps(&changes), [(1000 * G, 1029 * G + G / 2)]);
    assert!(
        daemon.text.contains("memory = \"1029.5G\""),
        "{}",
        daemon.text
    );
}

/// jvm lowered from 14G × 2 to 10G × 3 ten days ago, then the ledger's test, which succeeded at 14G, failed 3 times
/// in a row at 10G on two branches with no kernel event: a heap sized from the cgroup's limit.
fn heap_failures() -> History {
    let mut after = clear_at_ten(LOWERED + HOUR, 10);
    let at = NOW - 5 * HOUR;
    for (i, branch) in [(0, "main"), (1, "fix/parser"), (2, "main")] {
        after.push(failed(ledger(at + i * HOUR, 10 * G, 7 * G), branch, at + i));
    }
    lowered(after)
}

#[test]
fn a_revert_the_ceiling_held_short_finishes_once_the_ceiling_is_removed_across_a_restart() {
    let text = host(&jvm_at("10G", 3), "memory_ceiling = \"12.4G\"");
    let mut daemon = Daemon::restarted(text, heap_failures(), 64 * G, NOW - HOUR);
    let partial = daemon.checked_look(NOW);
    assert_eq!(
        memory_steps(&partial),
        [(10 * G, 12032 << 20)],
        "as far as the ceiling leaves"
    );

    // A restart reads only the file and the history; the revert is still owed, and the ceiling still holds it.
    let mut daemon = Daemon::restarted(daemon.text, daemon.history, 64 * G, NOW + HOUR);
    assert_eq!(daemon.checked_look(NOW + 2 * HOUR), []);
    let sizing = daemon.sizing();
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert!(
        jvm.held().is_some(),
        "held at the ceiling, not done: {:?}",
        jvm.advice
    );
    assert!(
        sizing.advice_line(jvm).contains("its jobs need 14G")
            && sizing
                .advice_line(jvm)
                .contains("since auto sizing lowered it from 14G"),
        "{}",
        sizing.advice_line(jvm)
    );
    let held = (daemon.text.clone(), daemon.history.clone());

    // With no new failure and no run at the cap, a reload without the ceiling finishes the revert at the next look.
    daemon.text = daemon.text.replace("memory_ceiling = \"12.4G\"\n", "");
    daemon.announce();
    let finished = daemon.checked_look(NOW + 3 * HOUR);
    assert_eq!(memory_steps(&finished), [(12032 << 20, 14 * G)]);
    assert_eq!(
        daemon.checked_look(NOW + 4 * HOUR),
        [],
        "nothing after it is done"
    );

    // A hand edit after the partial revert is the owner's: removing the ceiling does not take it back to 14G.
    let (text, history) = held;
    let mut daemon = Daemon::restarted(text, history, 64 * G, NOW + 2 * HOUR);
    daemon.set_by_hand(11 * G);
    daemon.text = daemon.text.replace("memory_ceiling = \"12.4G\"\n", "");
    daemon.announce();
    assert_eq!(memory_steps(&daemon.checked_look(NOW + 3 * HOUR)), []);
}

#[test]
fn a_revert_the_slice_cannot_hold_writes_nothing_and_says_the_slice_has_to_grow() {
    // Measured admission in an 11G slice: 10G beside the 512M mirror fits, the 11.75G the ceiling allows does not.
    let text = host(
        &jvm_at("10G", 3),
        "admission = \"measured\"\nmemory_ceiling = \"12.4G\"",
    );
    let mut daemon = Daemon::restarted(text, heap_failures(), 11 * G, NOW);
    for hour in 0..3 {
        assert_eq!(daemon.checked_look(NOW + hour * HOUR), [], "look {hour}");
    }
    let sizing = daemon.sizing();
    let jvm = sizing.profiles.iter().find(|p| p.name == "jvm").unwrap();
    assert_eq!(jvm.advice, Advice::SliceTooSmall { memory: 14 * G });
    assert!(jvm.held().is_none(), "the slice holds it, not the ceiling");
    assert!(
        sizing.advice_line(jvm).starts_with(&format!(
            "needs 14G ({} failed 3 times in a row at 10G since auto sizing lowered it from 14G), more than the \
             slice holds for one slot: raise the slice's MemoryMax first; [host] memory_ceiling then holds it at \
             11.75G",
            ledger_test()
        )),
        "{}",
        sizing.advice_line(jvm)
    );
}

#[test]
fn a_revert_under_caps_takes_back_only_the_slots_the_budget_holds_now() {
    // After the lowering the owner cut jvm to one slot and gave the room to android: 10G + 2 × 7G + 4G and the
    // mirror is 28.5G of a 33G slice. 14G × 2, the slots the lowering came from, would be 36.5G with the others.
    let android =
        "[profiles.android]\nimage_stage = \"node\"\ncpus = 8\nmemory = \"4G\"\nslots = 1\n\n";
    let text = jvm_at("10G", 1).replacen("[profiles.jvm]", &format!("{android}[profiles.jvm]"), 1);
    assert_eq!(check(&text, 33 * G), Ok(()));
    let mut daemon = Daemon::restarted(text.clone(), heap_failures(), 33 * G, NOW);
    let reverted = daemon.checked_look(NOW);
    assert_eq!(
        reverted.iter().map(|change| change.to).collect::<Vec<_>>(),
        [Resize {
            memory: 14 * G,
            slots: 1,
            cpus: None
        }],
        "back to 14G with the one slot that fits"
    );

    // When android's jobs waited and its look, before jvm's, takes the next 4G, the revert waits for room.
    let mut daemon = Daemon::restarted(text, heap_failures(), 33 * G, NOW);
    for i in 0..3 {
        let mut waited = super::super::super::job("halyard/app", "build", "android", 2 * G, 600);
        waited.queued_at += i * HOUR / 4;
        daemon.history.jobs.push(waited);
    }
    let changes = daemon.checked_look(NOW);
    assert_eq!(
        changes
            .iter()
            .map(|change| (change.profile.as_str(), change.to.memory, change.to.slots))
            .collect::<Vec<_>>(),
        [("android", 4 * G, 2)],
        "jvm's revert waits"
    );
}

#[test]
fn a_revert_max_memory_holds_under_the_ceiling_goes_to_max_memory_where_the_slice_holds_that() {
    // The slice holds one 11G jvm runner beside the 512M mirror (and, under caps, node's 14G), not the 11.75G the
    // ceiling allows, and max_memory stops the revert at 11G anyway, under either admission.
    for (admission, slots, memory_max) in
        [("measured", 3, 11 * G + G / 2), ("caps", 1, 25 * G + G / 2)]
    {
        let text = host(
            &jvm_at("10G", slots).replacen(
                &format!("slots = {slots}\n"),
                &format!("slots = {slots}\nmax_memory = \"11G\"\n"),
                1,
            ),
            &format!("admission = \"{admission}\"\nmemory_ceiling = \"12.4G\""),
        );
        let mut after = clear_at_ten(LOWERED + HOUR, 10);
        after.push(met(ledger(NOW - 2 * HOUR, 10 * G, 0)));
        let mut daemon = Daemon::restarted(text, lowered(after), memory_max, NOW - HOUR);
        let mut changes = Vec::new();
        for hour in 0..6 {
            changes.extend(daemon.checked_look(NOW + hour * HOUR));
        }
        assert_eq!(
            memory_steps(&changes),
            [(10 * G, 11 * G)],
            "{admission}: to max_memory, once"
        );
    }
}

#[test]
fn a_revert_the_owner_set_back_by_hand_stays_cancelled_after_a_later_raise() {
    let mut daemon = Daemon::restarted(
        jvm_at("10G", 3),
        lowered(clear_at_ten(LOWERED + HOUR, 10)),
        64 * G,
        NOW - 4 * HOUR,
    );
    // The owner tries 9G, then goes back to 10G by hand.
    daemon.now = NOW - 3 * HOUR;
    daemon.set_by_hand(9 * G);
    daemon.now = NOW - 2 * HOUR;
    daemon.set_by_hand(10 * G);
    // A run meets 10G: the lowering went too far, but the owner's 10G is raised as any hand-set cap is.
    daemon.history.jobs.push(met(ledger(NOW - HOUR, 10 * G, 0)));
    let raised = daemon.checked_look(NOW);
    assert_eq!(
        memory_steps(&raised),
        [(10 * G, 11776 << 20)],
        "to what the job needs"
    );
    assert_eq!(
        daemon.checked_look(NOW + 5 * 60 * 1000),
        [],
        "no revert to 14G next look"
    );
    let mut daemon = Daemon::restarted(daemon.text, daemon.history, 64 * G, NOW + 10 * 60 * 1000);
    assert_eq!(
        daemon.checked_look(NOW + 15 * 60 * 1000),
        [],
        "nor after a restart"
    );
}
