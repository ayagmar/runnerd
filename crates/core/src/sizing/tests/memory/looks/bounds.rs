//! A profile's `max_memory` and `min_memory` bound the advice as the config check bounds a profile (ADR 0088): the
//! advice `runnerctl sizing apply` and the dashboard's editor write, and auto sizing's changes, pass the check; a
//! profile held at its `max_memory` proposes nothing until a reload moves it, and its line says so.

use super::super::{LOWERED, clear_at_ten, ledger_test, lowered};
use super::ceiling::{check, host};
use super::{Daemon, G, HOUR, M, NOW, SizingChange, jvm_at, ledger, met};
use crate::api::{Advice, ProfileSizing, Tone};
use crate::config::{ByteSize, resize_profile};

/// `text` with jvm's `memory` line followed by `line`.
fn jvm_bound(text: &str, line: &str) -> String {
    text.replacen("memory = \"10G\"", &format!("memory = \"10G\"\n{line}"), 1)
}

impl Daemon {
    /// jvm's advice now.
    fn jvm(&self) -> ProfileSizing {
        self.sizing()
            .profiles
            .into_iter()
            .find(|profile| profile.name == "jvm")
            .unwrap()
    }

    /// What `runnerctl sizing apply jvm` and the dashboard's `/config?resize=jvm` would write now, which the config
    /// check has to pass: the advice's change, written into the file as it is.
    fn applied_by_hand(&self) -> Option<u64> {
        let change = self.jvm().advice.change()?;
        let written = resize_profile(&self.text, "jvm", change).unwrap();
        if let Err(error) = check(&written, self.memory_max) {
            panic!("applying jvm's advice writes a config the check refuses: {error}");
        }
        Some(change.memory)
    }

    /// Every 2 hours from `start`, `count` times, a ledger test run that meets jvm's cap, then a look; before each
    /// look, the advice applied by hand passes the check.
    fn meeting_the_cap(&mut self, start: u64, count: u64) -> Vec<SizingChange> {
        let mut changes = Vec::new();
        for i in 0..count {
            let at = start + i * 2 * HOUR;
            let cap = self.cap();
            self.history.jobs.push(met(ledger(at, cap, 0)));
            self.now = at;
            self.applied_by_hand();
            changes.extend(self.checked_look(at + HOUR));
        }
        changes
    }
}

/// jvm's memory changes among `changes`, as (from, to, slots).
fn steps(changes: &[SizingChange]) -> Vec<(u64, u64, u32)> {
    changes
        .iter()
        .filter(|change| change.profile == "jvm" && change.to.memory != change.from.memory)
        .map(|change| (change.from.memory, change.to.memory, change.to.slots))
        .collect()
}

/// What jvm held at a bound says, and what lets it grow.
struct Held {
    /// The words for the bound.
    holds: &'static str,
    /// Its advice in a table cell.
    word: &'static str,
    /// How its advice line ends.
    then: &'static str,
    /// Doctor's fix.
    fix: &'static str,
}

const BY_MAX_MEMORY: Held = Held {
    holds: "its max_memory",
    word: "at max_memory",
    then: "raise max_memory to give them more",
    fix: "raise max_memory in [profiles.jvm], then: runnerctl sizing apply jvm",
};

const BY_CEILING: Held = Held {
    holds: "the most the host's 12.4G memory_ceiling leaves one runner beside the mirror's 512M",
    word: "at ceiling",
    then: "raise [host] memory_ceiling if the host can grow further",
    fix: "raise [host] memory_ceiling if the host can grow further, then: runnerctl sizing apply jvm",
};

/// What a case bounds jvm by.
struct Bounds {
    name: &'static str,
    max_memory: &'static str,
    ceiling: Option<&'static str>,
    /// The most jvm's `memory` goes to.
    bound: u64,
    held: Held,
}

/// What a 10G jvm runs meet the cap of: 11.5G, then 13.25G.
const NEED: u64 = 11776 * M;

/// jvm at 10G × 2 under `bounds`, its runs meeting the cap for 4 days of looks: it goes to the bound, the slots
/// fitted at it, says what holds it there, and proposes nothing more until a reload raises `max_memory`.
fn raised_to_a_bound(admission: &str, bounds: &Bounds) {
    let case = format!("{admission}, {}", bounds.name);
    let bound = bounds.bound;
    let ceiling_line = bounds.ceiling.map_or(String::new(), |ceiling| {
        format!("\nmemory_ceiling = \"{ceiling}\"")
    });
    let text = host(
        &jvm_bound(
            &jvm_at("10G", 2),
            &format!("max_memory = \"{}\"", bounds.max_memory),
        ),
        &format!("admission = \"{admission}\"{ceiling_line}"),
    );
    let mut daemon = Daemon::new(text, NOW);
    // Under caps the slice holds node's 14G, the mirror and 2 jvm runners at 11G, not at 11.5G.
    if admission == "caps" {
        daemon.memory_max = 37 * G;
    }
    for i in 0..12 {
        let at = NOW - 30 * HOUR + i * 2 * HOUR;
        daemon.history.jobs.push(ledger(at, 10 * G, 6 * G));
    }
    let changes = daemon.meeting_the_cap(NOW, 48);
    let expected = match admission {
        _ if bound < NEED => vec![(10 * G, bound, 2)],
        "caps" => vec![(10 * G, NEED, 1), (NEED, bound, 1)],
        _ => vec![(10 * G, NEED, 2), (NEED, bound, 2)],
    };
    assert_eq!(
        steps(&changes),
        expected,
        "{case}: to the bound, then nothing more"
    );
    let last = changes
        .iter()
        .rfind(|change| change.to.memory == bound)
        .unwrap();
    let held = &bounds.held;
    assert!(
        last.reason.starts_with(&format!(
            "raise memory to {}, {}, short of the ",
            ByteSize(bound),
            held.holds
        )),
        "{case}: {}",
        last.reason
    );

    let jvm = daemon.jvm();
    let sizing = daemon.sizing();
    assert!(jvm.held().is_some(), "{case}: jvm is held");
    assert_eq!(jvm.advice_word(), held.word, "{case}");
    assert_eq!(jvm.advice.tone(), Tone::Bad, "{case}: its jobs need more");
    assert_eq!(daemon.applied_by_hand(), None, "{case}: nothing to apply");
    let line = sizing.advice_line(&jvm);
    assert!(
        line.starts_with(&format!(
            "keep {}, {}: its jobs need ",
            ByteSize(bound),
            held.holds
        )) && line.ends_with(held.then),
        "{case}: {line}"
    );
    let fix = sizing.doctor_check().fix.unwrap_or_default();
    assert!(fix.contains(held.fix), "{case}: {fix}");

    // A reload that raises max_memory lets the next look raise, unless the ceiling still holds it.
    daemon.text = daemon.text.replace(
        &format!("max_memory = \"{}\"", bounds.max_memory),
        "max_memory = \"20G\"",
    );
    daemon.announce();
    let raised = daemon.meeting_the_cap(NOW + 100 * HOUR, 1);
    let raised: Vec<u64> = steps(&raised).into_iter().map(|(_, to, _)| to).collect();
    if bound < NEED {
        assert!(
            matches!(raised[..], [to] if to > bound),
            "{case}: raised past the old max_memory: {raised:?}"
        );
    } else {
        assert!(
            raised.is_empty(),
            "{case}: the ceiling still holds it: {raised:?}"
        );
    }
}

#[test]
fn a_raise_goes_no_further_than_max_memory_or_the_ceiling_whichever_is_lower_and_then_proposes_nothing()
 {
    // The config check refuses a cap already over max_memory, so no advice starts from one.
    let over = jvm_bound(&jvm_at("10G", 2), "max_memory = \"11G\"").replacen(
        "memory = \"10G\"",
        "memory = \"12G\"",
        1,
    );
    assert!(
        check(&over, 64 * G).is_err_and(|error| error.contains("memory is above its max_memory")),
        "a cap over max_memory is refused"
    );
    let cases = [
        Bounds {
            name: "max_memory",
            max_memory: "11G",
            ceiling: None,
            bound: 11 * G,
            held: BY_MAX_MEMORY,
        },
        Bounds {
            name: "max_memory under the ceiling",
            max_memory: "11G",
            ceiling: Some("12.4G"),
            bound: 11 * G,
            held: BY_MAX_MEMORY,
        },
        Bounds {
            name: "the ceiling under max_memory",
            max_memory: "12.5G",
            ceiling: Some("12.4G"),
            bound: 12032 * M,
            held: BY_CEILING,
        },
        // Raising max_memory alone would not let it past the ceiling: the ceiling is named.
        Bounds {
            name: "max_memory at the ceiling's bound",
            max_memory: "11.75G",
            ceiling: Some("12.4G"),
            bound: 12032 * M,
            held: BY_CEILING,
        },
    ];
    for admission in ["caps", "measured"] {
        for bounds in &cases {
            raised_to_a_bound(admission, bounds);
        }
    }
}

#[test]
fn a_revert_past_max_memory_goes_back_as_far_as_it_allows_and_finishes_once_it_is_raised() {
    // jvm was lowered from 14G × 2 to 10G × 3, then the owner set max_memory to 12G, under where it came from.
    let mut after = clear_at_ten(LOWERED + HOUR, 10);
    after.push(met(ledger(NOW - 2 * HOUR, 10 * G, 0)));
    let text = jvm_bound(&jvm_at("10G", 3), "max_memory = \"12G\"");
    let mut daemon = Daemon::restarted(text, lowered(after), 64 * G, NOW - HOUR);
    let mut changes = Vec::new();
    for look in 0..6 {
        daemon.now = NOW + look * HOUR;
        daemon.applied_by_hand();
        changes.extend(daemon.checked_look(NOW + look * HOUR));
    }
    assert_eq!(
        steps(&changes),
        [(10 * G, 12 * G, 2)],
        "to max_memory, once"
    );
    assert_eq!(
        changes[0].reason,
        format!(
            "raise memory back to 12G, its max_memory: {} met the 10G cap since auto sizing lowered it from 14G",
            ledger_test()
        )
    );
    let jvm = daemon.jvm();
    assert_eq!(jvm.advice, Advice::SliceTooSmall { memory: 14 * G });
    assert_eq!(jvm.advice_word(), "at max_memory");
    assert_eq!(
        daemon.sizing().advice_line(&jvm),
        format!(
            "keep 12G, its max_memory: its jobs need 14G ({} met the 10G cap since auto sizing lowered it from \
             14G); raise max_memory to give them more",
            ledger_test()
        )
    );

    // A reload that raises max_memory finishes the revert at the next look, with no new run at the cap.
    daemon.text = daemon
        .text
        .replace("max_memory = \"12G\"", "max_memory = \"16G\"");
    daemon.announce();
    let finished = daemon.checked_look(NOW + 7 * HOUR);
    assert_eq!(steps(&finished), [(12 * G, 14 * G, 2)]);
    assert_eq!(daemon.checked_look(NOW + 8 * HOUR), [], "then nothing more");
}

#[test]
fn a_lowering_stops_at_min_memory_and_then_keeps_it_saying_what_holds_it() {
    for admission in ["caps", "measured"] {
        let text = host(
            &jvm_bound(&jvm_at("10G", 2), "min_memory = \"8G\""),
            &format!("admission = \"{admission}\""),
        );
        let mut daemon = Daemon::new(text, NOW - 2 * 24 * HOUR);
        // Clear 4G runs: they need 5G, with 1G for page cache.
        for i in 0..12 {
            let at = NOW - 40 * HOUR + i * HOUR;
            daemon.history.jobs.push(ledger(at, 10 * G, 4 * G));
        }
        daemon.now = NOW;
        let jvm = daemon.jvm();
        assert_eq!(
            jvm.advice,
            Advice::Lower {
                memory: 8 * G,
                slots: 2
            },
            "{admission}"
        );
        let line = daemon.sizing().advice_line(&jvm);
        assert!(
            line.starts_with(
                "lower memory to 8G from 10G, its min_memory: its largest peak + 1G for page cache is 5G"
            ),
            "{admission}: {line}"
        );
        assert_eq!(daemon.applied_by_hand(), Some(8 * G), "{admission}");
        let mut changes = Vec::new();
        for day in 0..4 {
            changes.extend(daemon.checked_look(NOW + day * 25 * HOUR));
        }
        // 25% of 10G is 2.5G: one step reaches 8G.
        assert_eq!(
            steps(&changes),
            [(10 * G, 8 * G, 2)],
            "{admission}: to min_memory, once"
        );

        let jvm = daemon.jvm();
        assert_eq!(jvm.advice, Advice::Keep { memory: 5 * G }, "{admission}");
        assert_eq!(jvm.advice.tone(), Tone::Good, "{admission}");
        assert_eq!(
            daemon.applied_by_hand(),
            None,
            "{admission}: nothing to apply"
        );
        assert_eq!(
            daemon.sizing().advice_line(&jvm),
            "keep 8G, its min_memory: its largest peak + 1G for page cache is 5G",
            "{admission}"
        );
    }
}
