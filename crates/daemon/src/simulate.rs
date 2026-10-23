//! `runnerd simulate`: the job history replayed through admission by caps and by measured peaks, with and without
//! a CPU budget (`admission::simulate`), as a table. Reads the history database read-only, as `runnerd doctor` does; changes
//! nothing.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use runnerd_core::admission::{Policy, Replay, simulate};
use runnerd_core::api;
use runnerd_core::config::{self, ByteSize, Config};
use runnerd_core::sizing::Ran;

/// One replay to run: a policy, the slots it may use and the CPU budget in millicores, if any.
struct Row {
    name: String,
    policy: Policy,
    slots: BTreeMap<String, u32>,
    cpu_budget: Option<u64>,
}

/// The slice's CPUs, in millicores, and the overcommit factors to replay a CPU budget with.
pub(crate) struct Cpus<'factors> {
    pub(crate) millicores: u64,
    pub(crate) overcommit: &'factors [f64],
}

/// The table for `jobs`: caps with the configured slots, then caps and measured admission with `slots` (by
/// profile; where absent, as many as the slice holds of the profile alone at its cap), then measured admission with
/// a CPU budget of the slice's CPUs times each overcommit factor.
pub(crate) fn report(
    config: &Config,
    memory_max: Option<u64>,
    cpus: &Cpus<'_>,
    jobs: &[Ran],
    slots: &BTreeMap<String, u32>,
) -> String {
    let rows = rows(config, memory_max, cpus, slots);
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
         ever held, but a lower bound on what the daemon could overlap. The replay holds each running job at its \
         peak, where the daemon admits against what jobs use while they warm up, and it leaves out start latency, \
         seeds, shells and cancelled jobs.\n"
    );
    let _ = writeln!(
        out,
        "{:<9} {:<22} {:>9} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7} {:>5} {:>6} {:>8}",
        "POLICY",
        "SLOTS",
        "WAIT P50",
        "WAIT P95",
        "WAIT MAX",
        "MEAN",
        "AT ONCE",
        "PEAKS",
        "UNDER",
        "HELD",
        "CPU",
        "CPU HELD"
    );
    for row in rows {
        let replay = simulate(
            config,
            memory_max,
            row.cpu_budget,
            jobs,
            row.policy,
            &row.slots,
        );
        let _ = writeln!(out, "{}", line(&row, &replay, memory_max));
    }
    let _ = write!(
        out,
        "\n* more slots than the caps fit in MemoryMax, which the config check refuses with admission = \"caps\".\n\
         UNDER: jobs that peaked above what admission reserved for them. HELD: times the high-water mark held back \
         a job the predictions had room for.\n\
         cpu ×N: measured admission, and each runner reserves its job's predicted average CPU of the slice's {} \
         CPUs times N. CPU: the most the running jobs' average CPUs added up to at once. CPU HELD: times the CPU \
         budget held back a job memory and its slots had room for. The replay keeps each job's run time, so it \
         shows what a CPU budget holds back, not how much sharing the CPUs slows the jobs.",
        api::cores(cpus.millicores)
    );
    out
}

/// The replays `report` runs, as its doc lists them.
fn rows(
    config: &Config,
    memory_max: Option<u64>,
    cpus: &Cpus<'_>,
    slots: &BTreeMap<String, u32>,
) -> Vec<Row> {
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
    let mut rows = vec![
        Row {
            name: "caps".to_owned(),
            policy: Policy::Caps,
            slots: configured,
            cpu_budget: None,
        },
        Row {
            name: "caps*".to_owned(),
            policy: Policy::Caps,
            slots: raised.clone(),
            cpu_budget: None,
        },
        Row {
            name: "measured".to_owned(),
            policy: Policy::Measured,
            slots: raised.clone(),
            cpu_budget: None,
        },
    ];
    rows.extend(cpus.overcommit.iter().map(|factor| Row {
        name: format!("cpu ×{factor}"),
        policy: Policy::Measured,
        slots: raised.clone(),
        cpu_budget: Some(config::cpu_budget(cpus.millicores, *factor)),
    }));
    rows
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
        "{:<9} {:<22} {:>9} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7} {:>5} {:>6} {:>8}",
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
        api::cores(replay.worst_cpu),
        if row.cpu_budget.is_some() {
            replay.cpu_held.to_string()
        } else {
            "-".to_owned()
        },
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

/// Parses `--cpus 7.5` as millicores.
pub(crate) fn cpus(text: &str) -> Result<u64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|cpus| cpus.is_finite() && *cpus > 0.0)
        .map(|cpus| config::cpu_budget(1000, cpus))
        .ok_or_else(|| format!("{text:?} is not a number of CPUs above 0"))
}

/// Parses `--cpu-overcommit 1.5`, as the config check takes it.
pub(crate) fn overcommit(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|factor| *factor > 0.0 && *factor <= config::MAX_CPU_OVERCOMMIT)
        .ok_or_else(|| {
            format!(
                "{text:?} is not a factor above 0 and at most {}",
                config::MAX_CPU_OVERCOMMIT
            )
        })
}
