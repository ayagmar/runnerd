//! `runnerd simulate`: the job history replayed through admission by caps and by measured peaks
//! (`admission::simulate`), as a table. Reads the history database read-only, as `runnerd doctor` does; changes
//! nothing.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use runnerd_core::admission::{Policy, Replay, simulate};
use runnerd_core::config::{ByteSize, Config};
use runnerd_core::sizing::Ran;

/// One replay to run: a policy and the slots it may use.
struct Row {
    name: &'static str,
    policy: Policy,
    slots: BTreeMap<String, u32>,
}

/// The table for `jobs`: caps with the configured slots, then caps and measured admission with `slots` (by
/// profile; where absent, as many as the slice holds of the profile alone at its cap).
pub(crate) fn report(
    config: &Config,
    memory_max: Option<u64>,
    jobs: &[Ran],
    slots: &BTreeMap<String, u32>,
) -> String {
    let configured: BTreeMap<String, u32> = config
        .profiles
        .iter()
        .map(|(name, profile)| (name.clone(), profile.slots))
        .collect();
    let mirror = if config.caches.mirror.enabled {
        config.caches.mirror.memory.0
    } else {
        0
    };
    let raised: BTreeMap<String, u32> = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let alone = memory_max.map_or(profile.slots, |max| {
                u32::try_from(max.saturating_sub(mirror) / profile.memory.0.max(1))
                    .unwrap_or(u32::MAX)
            });
            (
                name.clone(),
                slots.get(name).copied().unwrap_or(alone.max(profile.slots)),
            )
        })
        .collect();
    let rows = [
        Row {
            name: "caps",
            policy: Policy::Caps,
            slots: configured,
        },
        Row {
            name: "caps*",
            policy: Policy::Caps,
            slots: raised.clone(),
        },
        Row {
            name: "measured",
            policy: Policy::Measured,
            slots: raised,
        },
    ];
    let mut out = String::new();
    let budget = memory_max.map_or_else(
        || "an unlimited slice".to_owned(),
        |max| format!("a MemoryMax of {}", ByteSize(max)),
    );
    let _ = writeln!(
        out,
        "{} finished jobs replayed against {budget}, each queued when it was and running as long as it ran.",
        jobs.len()
    );
    let _ = writeln!(
        out,
        "PEAKS is every running job's peak added up at the busiest moment, with the mirror: more than the slice \
         ever held.\n"
    );
    let _ = writeln!(
        out,
        "{:<9} {:<22} {:>9} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7} {:>5}",
        "POLICY",
        "SLOTS",
        "WAIT P50",
        "WAIT P95",
        "WAIT MAX",
        "MEAN",
        "AT ONCE",
        "PEAKS",
        "UNDER",
        "HELD"
    );
    for row in rows {
        let replay = simulate(config, memory_max, jobs, row.policy, &row.slots);
        let _ = writeln!(out, "{}", line(&row, &replay, memory_max));
    }
    out.push_str(
        "\n* more slots than the caps fit in MemoryMax, which the config check refuses with admission = \"caps\".\n\
         UNDER: jobs that peaked above what admission reserved for them. HELD: times the high-water mark held back \
         a job the predictions had room for.",
    );
    out
}

fn line(row: &Row, replay: &Replay, memory_max: Option<u64>) -> String {
    let slots = row
        .slots
        .iter()
        .map(|(profile, slots)| format!("{profile} {slots}"))
        .collect::<Vec<_>>()
        .join(", ");
    let over = memory_max.is_some_and(|max| replay.worst_overlap > max);
    let mut line = format!(
        "{:<9} {:<22} {:>9} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7} {:>5}",
        row.name,
        slots,
        duration(replay.wait.p50),
        duration(replay.wait.p95),
        duration(replay.wait.max),
        duration(replay.mean_wait),
        replay.most_running,
        format!(
            "{}{}",
            ByteSize(replay.worst_overlap),
            if over { "!" } else { "" }
        ),
        replay.under_predicted,
        replay.held_back,
    );
    if replay.never_started > 0 {
        let _ = write!(line, "  {} never started", replay.never_started);
    }
    line
}

/// Seconds as `4m20s`, `2h05m` or `35s`.
fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Parses `--slots node=4`.
pub(crate) fn slots(text: &str) -> Result<(String, u32), String> {
    let (profile, slots) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not PROFILE=SLOTS"))?;
    let slots = slots
        .parse()
        .map_err(|_| format!("{slots:?} is not a number of slots"))?;
    Ok((profile.to_owned(), slots))
}
