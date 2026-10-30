//! `[host] memory_ceiling` bounds what auto sizing writes as the config check bounds it (ADR 0086): a raise or a
//! revert goes no further than the ceiling leaves one runner beside the mirror, every config it writes passes the
//! check, and a look at the bound proposes nothing new until a reload moves the ceiling or the mirror.

use super::super::{LOWERED, clear_at_ten, ledger_test, look, lowered, sizing};
use super::{Daemon, G, HOUR, NOW, SizingChange, jvm_at, ledger, met, window};
use crate::api::{Advice, Resize, Sizing};
use crate::config::{ByteSize, Config, resize_profile};
use crate::sizing::advise;

impl Daemon {
    /// The advice as `runnerctl sizing` shows it after the last look.
    fn sizing(&self) -> Sizing {
        advise(
            &self.config(),
            Some(64 * G),
            &self.history,
            window(self.now),
        )
    }
}

/// `text` with `line` added under `[host]`.
fn host(text: &str, line: &str) -> String {
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

/// Whether the config check of the 64G slice the looks run against passes `text`, and why not.
fn check(text: &str) -> Result<(), String> {
    Config::parse(text)
        .map_err(|error| error.to_string())?
        .validate(Some(ByteSize(64 * G)))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// jvm's memory changes among `changes`, as (from, to).
fn memory_steps(changes: &[SizingChange]) -> Vec<(u64, u64)> {
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
        changes.extend(daemon.look(at + HOUR));
        if let Err(error) = check(&daemon.text) {
            panic!("the look at {at} wrote a config the check refuses: {error}");
        }
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
            assert!(jvm.at_ceiling(), "{case}: jvm is held at the ceiling");
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
    assert_eq!(check(&written), Ok(()), "the revert passes the check");
}

#[test]
fn a_revert_the_ceiling_leaves_no_room_for_changes_nothing_and_keeps_its_evidence() {
    // The ceiling leaves one runner exactly the 10G jvm was lowered to.
    let mut after = clear_at_ten(LOWERED + HOUR, 10);
    after.push(met(ledger(NOW - 2 * HOUR, 10 * G, 0)));
    let history = lowered(after);
    let text = host(&jvm_at("10G", 3), "memory_ceiling = \"10.5G\"");
    assert_eq!(check(&text), Ok(()), "10G and the mirror fill the ceiling");
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
