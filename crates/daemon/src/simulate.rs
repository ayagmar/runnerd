//! `runnerd simulate`: the job history replayed through admission by caps and by measured peaks, with and without
//! a CPU budget (`admission::simulate`), as a table; and with `--priority`, with no barrier for a starved job, a strict
//! one (ADR 0096) and the daemon's (ADR 0097), pool by pool and run by run; and with `--causal`, by each job's own samples
//! under the plain and the pooled count, with what each risks (ADR 0099). Reads the history database read-only, as
//! `runnerd doctor` does; changes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use runnerd_core::admission::trace::{Episodes, Trace};
use runnerd_core::admission::{
    Arrivals, Band, Barriers, CreditRoom, Policy, PoolReplay, Replay, Replaying, pooled, simulate,
};
use runnerd_core::api;
use runnerd_core::config::{self, Admission, ByteSize, Config};
use runnerd_core::sizing::{History, LeftOut, Ran};

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
            Barriers::On,
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

/// What `priority` replays: the config's policy and CPU budget, with its slots but where `slots` says otherwise.
pub(crate) struct Comparison<'run> {
    pub(crate) memory_max: Option<u64>,
    /// The CPU budget, in millicores.
    pub(crate) cpu_budget: Option<u64>,
    pub(crate) jobs: &'run [Ran],
    /// What the daemon had read by the time those jobs queued.
    pub(crate) known: &'run History,
    pub(crate) slots: &'run BTreeMap<String, u32>,
    pub(crate) left_out: LeftOut,
}

/// The same replay with no barrier, with a barrier nothing starts behind, and as the daemon runs (ADR 0097), side by
/// side: each pool and profile by its jobs' waits, then whole runs, then the budget's busy share and what a barrier
/// withheld. Jobs that followed another in their run arrive after its replayed end, so a job held back also holds back
/// what comes after it.
pub(crate) fn priority(config: &Config, comparison: &Comparison<'_>) -> String {
    let slots: BTreeMap<String, u32> = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let slots = comparison.slots.get(name).copied();
            (name.clone(), slots.unwrap_or(profile.slots))
        })
        .collect();
    let policy = match config.host.admission {
        Admission::Caps => Policy::Caps,
        Admission::Measured => Policy::Measured,
    };
    let replay = |barriers| {
        simulate(
            config,
            comparison.memory_max,
            comparison.cpu_budget,
            comparison.jobs,
            policy,
            &slots,
            Replaying {
                barriers,
                arrivals: Arrivals::AfterNeeds,
                known: Some(comparison.known),
                trace: None,
                most_credit: 0,
                host_hold: u64::MAX,
                credit_room: CreditRoom::Counted,
                band: Band::NoWaiterFits,
            },
        )
    };
    let replays = [
        replay(Barriers::Off),
        replay(Barriers::Strict),
        replay(Barriers::On),
    ];
    let [without, strict, with] = &replays;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n\nWith no barrier for a starved job, a strict one, and one younger jobs may start behind (the daemon's). A job \
         that queued after another of its run ended arrives as long after that job's replayed end.\n"
    );
    out.push_str(&waits_and_runs(&replays, &slots));
    let _ = writeln!(
        out,
        "\nBusy share of the budget, what the running jobs took over the budget's time: {} with no barrier, {} strict, \
         {} with younger jobs behind it.",
        share(without),
        share(strict),
        share(with)
    );
    let _ = writeln!(
        out,
        "A strict barrier stood for {}, with {} GiB-minutes of the budget free meanwhile; with younger jobs behind it, \
         {} and {}: at most what it withheld, as a job behind it may not have fit that room.",
        duration(strict.barrier_for / 1000),
        gib_minutes(strict.withheld),
        duration(with.barrier_for / 1000),
        gib_minutes(with.withheld)
    );
    let _ = writeln!(
        out,
        "{} jobs started behind a barrier; {} ran past the usual run time they passed by, by {} at most.",
        with.backfilled.jobs,
        with.backfilled.overran,
        duration(with.backfilled.most_overrun)
    );
    let LeftOut {
        queued,
        running,
        cancelled,
        elsewhere,
    } = comparison.left_out;
    let _ = write!(
        out,
        "Not replayed, having no run time to replay: {queued} jobs still queued, {running} running, {cancelled} \
         cancelled and {elsewhere} taken elsewhere. Those still queued may be the ones that waited longest; no figure \
         here says what the barrier does for them. The replay has no host readings, warm runners, restarts or \
         recoveries, a job gets the runner that starts for it, where GitHub may give that runner any job of its pool, \
         and what follows what is read from the times."
    );
    out
}

/// How `causal` counts: the credits it replays, a row each, and what of each the host's bound leaves out, all of it as
/// the daemon's does at `u64::MAX`.
pub(crate) struct Counts<'credits> {
    /// The most credit of each row, in bytes: 0 is the plain count.
    pub(crate) credits: &'credits [u64],
    pub(crate) host_hold: u64,
    /// What holds up the host's larger margin.
    pub(crate) band: Band,
    /// Whether to also compare what a barrier may do beside `pooled::MOST_CREDIT`.
    pub(crate) barriers: Comparing,
}

/// Whether a report also compares the barrier for a starved job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comparing {
    Priority,
    Policies,
}

/// The same replay as the jobs really ran (`Replaying::trace`) with each credit of `counts`: waits, runs, what held
/// jobs, and what each count risked. ADR 0099 reads these tables.
pub(crate) fn causal(
    config: &Config,
    comparison: &Comparison<'_>,
    trace: &Trace,
    counts: &Counts<'_>,
) -> String {
    let slots: BTreeMap<String, u32> = config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let slots = comparison.slots.get(name).copied();
            (name.clone(), slots.unwrap_or(profile.slots))
        })
        .collect();
    let policy = match config.host.admission {
        Admission::Caps => Policy::Caps,
        Admission::Measured => Policy::Measured,
    };
    let replay = |barriers, most_credit, credit_room| {
        simulate(
            config,
            comparison.memory_max,
            comparison.cpu_budget,
            comparison.jobs,
            policy,
            &slots,
            Replaying {
                barriers,
                arrivals: Arrivals::AfterNeeds,
                known: Some(comparison.known),
                trace: Some(trace),
                most_credit,
                host_hold: counts.host_hold,
                credit_room,
                band: counts.band,
            },
        )
    };
    let replays: Vec<Replay> = counts
        .credits
        .iter()
        .map(|credit| replay(Barriers::On, *credit, CreditRoom::Counted))
        .collect();
    let mut out = String::new();
    let sampled = comparison
        .jobs
        .iter()
        .filter(|run| {
            trace
                .runs
                .contains_key(&runnerd_core::admission::trace::RunId::of(run))
        })
        .count();
    let _ = writeln!(
        out,
        "\n\nAs the jobs really ran: {sampled} of {} with samples, the slice holding {} beside its runners and read at {} \
         by the guards. A credit of 0B is the plain count.\n",
        comparison.jobs.len(),
        ByteSize(trace.overhead),
        ByteSize(trace.guard_overhead)
    );
    let _ = writeln!(
        out,
        "{:<6} {:>9} {:>9} {:>10} {:>9} {:>9} {:>9} {:>7}",
        "CREDIT", "RUN MEAN", "RUN WORST", "NEVER", "WAIT H", "HOST H", "SLICE H", "BEHIND"
    );
    for (credit, replay) in counts.credits.iter().zip(&replays) {
        let _ = writeln!(
            out,
            "{}",
            causal_line(&ByteSize(*credit).to_string(), replay)
        );
    }
    if counts.barriers == Comparing::Priority {
        let _ = writeln!(
            out,
            "\nBeside a credit of {}:\n",
            ByteSize(pooled::MOST_CREDIT)
        );
        let most = pooled::MOST_CREDIT;
        for (name, barriers, room) in [
            ("this rule", Barriers::On, CreditRoom::Counted),
            ("credit not counted", Barriers::On, CreditRoom::LeftOut),
            ("strict", Barriers::Strict, CreditRoom::Counted),
        ] {
            let _ = writeln!(out, "{}", causal_line(name, &replay(barriers, most, room)));
        }
    }
    out.push_str(&causal_profiles(counts.credits, &replays, &slots));
    out.push_str(&causal_waits(&replays));
    out.push_str(&causal_hazards(counts.credits, &replays));
    out
}

/// `replay` as a row of `causal`'s first table.
fn causal_line(name: &str, replay: &Replay) -> String {
    let runs = runs_of(replay, None);
    format!(
        "{name:<6} {:>9} {:>9} {:>10} {:>9} {:>9} {:>9} {:>7}",
        duration(mean(&runs)),
        duration(runs.iter().copied().max().unwrap_or(0)),
        replay.never_started,
        job_hours(1000 * replay.mean_wait * (replay.jobs - replay.never_started)),
        job_hours(replay.host_waited),
        job_hours(replay.slice_waited),
        replay.backfilled.jobs,
    )
}

/// The runs with a job of each profile of `slots` in `replays`, mean / worst, a column a credit.
fn causal_profiles(credits: &[u64], replays: &[Replay], slots: &BTreeMap<String, u32>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nRuns with a job of each profile, mean / worst:\n");
    let mut heading = format!("{:<36}", "CREDIT");
    for credit in credits {
        let _ = write!(heading, " {:>16}", ByteSize(*credit).to_string());
    }
    let _ = writeln!(out, "{heading}");
    for profile in slots.keys() {
        let mut line = format!("{profile:<36}");
        for replay in replays {
            let runs = runs_of(replay, Some(profile));
            let worst = runs.iter().copied().max().unwrap_or(0);
            let _ = write!(
                line,
                " {:>16}",
                format!("{} / {}", duration(mean(&runs)), duration(worst))
            );
        }
        let _ = writeln!(out, "{line}");
    }
    out
}

/// Each pool's waits in `replays`, a column a credit.
fn causal_waits(replays: &[Replay]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nEach pool's wait, mean / p95:\n");
    let names: BTreeSet<_> = replays
        .iter()
        .flat_map(|replay| replay.pools.keys())
        .collect();
    for pool in names {
        let mut line = format!("{:<36}", format!("{}/{}", pool.repository, pool.profile));
        for replay in replays {
            let of_pool = replay.pools.get(pool).cloned().unwrap_or_default();
            let wait = of_pool.wait();
            let _ = write!(
                line,
                " {:>16}",
                format!(
                    "{} / {}",
                    duration(mean(&of_pool.waits)),
                    duration(wait.p95)
                )
            );
        }
        let _ = writeln!(out, "{line}");
    }
    out
}

/// What each of `replays` risks, a row a credit.
fn causal_hazards(credits: &[u64], replays: &[Replay]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\nHazards (episodes, time, worst):\n\n{:<6} {:>9} {:>20} {:>20} {:>20} {:>9} {:>9} {:>9} {:>7} {:>7} {:>6} {:>9}",
        "CREDIT",
        "SLICE MAX",
        "PAST 90%",
        "PAST MAX",
        "USE PAST COUNT",
        "<20G",
        "<6.2G",
        "BAND",
        "LOWEST",
        "FALLS",
        "OVER",
        "CREDIT GH"
    );
    for (credit, replay) in credits.iter().zip(replays) {
        let hazard = &replay.hazard;
        let tenths = hazard.credit / (u128::from(1_u32 << 30) * 360_000);
        let _ = writeln!(
            out,
            "{:<6} {:>9} {:>20} {:>20} {:>20} {:>9} {:>9} {:>9} {:>7} {:>7} {:>6} {:>9}",
            ByteSize(*credit).to_string(),
            ByteSize(hazard.slice_peak).to_string(),
            episodes(hazard.over_mark),
            episodes(hazard.over_max),
            episodes(hazard.unfunded),
            duration(hazard.under_throttling / 1000),
            duration(hazard.under_oomd / 1000),
            duration(hazard.in_band / 1000),
            hazard
                .lowest_available
                .map_or_else(|| "-".to_owned(), |lowest| ByteSize(lowest).to_string()),
            hazard.credit_falls,
            hazard.overdrawn,
            format!("{}.{}", tenths / 10, tenths % 10),
        );
    }
    let active = replays.first().map_or(0, |replay| replay.hazard.active) / 1000;
    let _ = write!(
        out,
        "\n<20G and <6.2G: time with the host's MemAvailable under those lines, of {} with a runner running. BAND: time at the \
         host's larger margin. FALLS: times the \
         credit fell; OVER: those that left the count past a budget. The host's programs are as recorded and do not react \
         to a fuller slice; no pressure, no reseeds, shells, warm runners or restarts; a job's run time stays as it ran.",
        duration(active)
    );
    out
}

/// The mean of `values`.
fn mean(values: &[u64]) -> u64 {
    values.iter().sum::<u64>() / (values.len() as u64).max(1)
}

/// `waited` ms as job-hours, to a tenth.
fn job_hours(waited: u64) -> String {
    format!("{}.{}", waited / 3_600_000, waited / 360_000 % 10)
}

/// `spans` as `2 / 1m30s / 1.5G`.
fn episodes(spans: Episodes) -> String {
    format!(
        "{} / {} / {}",
        spans.count,
        duration(spans.time / 1000),
        ByteSize(spans.worst)
    )
}

/// The tables of `replays`: each pool and profile of `slots` by its jobs' waits, then whole runs.
fn waits_and_runs(replays: &[Replay; 3], slots: &BTreeMap<String, u32>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Each job's wait from arrival to start:\n");
    let _ = writeln!(out, "{}", heading("POOL", "JOBS", ["P50", "P95", "MAX"]));
    let names: BTreeSet<_> = replays
        .iter()
        .flat_map(|replay| replay.pools.keys())
        .collect();
    for pool in names {
        let of_pool = |replay: &Replay| replay.pools.get(pool).cloned().unwrap_or_default();
        let name = format!("{}/{}", pool.repository, pool.profile);
        let _ = writeln!(
            out,
            "{}",
            pool_line(&name, &replays.each_ref().map(of_pool))
        );
    }
    for profile in slots.keys() {
        let name = format!("all of {profile}");
        let _ = writeln!(
            out,
            "{}",
            pool_line(
                &name,
                &replays.each_ref().map(|replay| of_profile(replay, profile))
            )
        );
    }
    let _ = writeln!(
        out,
        "\nEach run attempt, from its first job's arrival to its last job's end:\n"
    );
    let _ = writeln!(out, "{}", heading("RUNS", "RUNS", ["MEAN", "P50", "MAX"]));
    let _ = writeln!(
        out,
        "{}",
        runs_line(
            "all",
            &replays.each_ref().map(|replay| runs_of(replay, None))
        )
    );
    for profile in slots.keys() {
        let name = format!("with a {profile} job");
        let _ = writeln!(
            out,
            "{}",
            runs_line(
                &name,
                &replays
                    .each_ref()
                    .map(|replay| runs_of(replay, Some(profile)))
            )
        );
    }
    out
}

/// Three columns for each of the replays, under `name` and `count`.
fn heading(name: &str, count: &str, columns: [&str; 3]) -> String {
    let group = |replay: &str| {
        let [first, second, third] = columns.map(|column| format!("{column} {replay}"));
        format!("{first:>14} {second:>14} {third:>14}")
    };
    format!(
        "{name:<36} {count:>6}  {}  {}  {}",
        group("NONE"),
        group("STRICT"),
        group("NOW")
    )
}

/// `name`, how many jobs or runs, and three figures for each of the replays.
fn figures_line(name: &str, count: usize, figures: [[u64; 3]; 3]) -> String {
    let group = |[first, second, third]: [u64; 3]| {
        format!(
            "{:>14} {:>14} {:>14}",
            duration(first),
            duration(second),
            duration(third)
        )
    };
    let [none, strict, now] = figures.map(group);
    format!("{name:<36} {count:>6}  {none}  {strict}  {now}")
}

/// One pool's or profile's figures in each of the replays, and how many jobs each never started, if any did.
fn pool_line(name: &str, replays: &[PoolReplay; 3]) -> String {
    let figures = replays.each_ref().map(|replay| {
        let wait = replay.wait();
        [wait.p50, wait.p95, wait.max]
    });
    let count = replays[0].waits.len() + usize::try_from(replays[0].never_started).unwrap_or(0);
    let mut line = figures_line(name, count, figures);
    let never = replays.each_ref().map(|replay| replay.never_started);
    if never != [0; 3] {
        let _ = write!(
            line,
            "  never started: {} / {} / {}",
            never[0], never[1], never[2]
        );
    }
    line
}

/// How long `runs` took, in each of the replays.
fn runs_line(name: &str, runs: &[Vec<u64>; 3]) -> String {
    let figures = runs.each_ref().map(|took| {
        let total: u64 = took.iter().sum();
        let spread = PoolReplay {
            waits: took.clone(),
            never_started: 0,
        }
        .wait();
        [total / (took.len() as u64).max(1), spread.p50, spread.max]
    });
    figures_line(name, runs[0].len(), figures)
}

/// How long the run attempts of `replay` took, in seconds: those with a job of `profile`, or all.
fn runs_of(replay: &Replay, profile: Option<&String>) -> Vec<u64> {
    replay
        .runs
        .iter()
        .filter(|run| profile.is_none_or(|profile| run.profiles.contains(profile)))
        .map(|run| run.took)
        .collect()
}

/// What the pools of `profile` did, together.
fn of_profile(replay: &Replay, profile: &str) -> PoolReplay {
    let mut all = PoolReplay::default();
    for (_, pool) in replay
        .pools
        .iter()
        .filter(|(pool, _)| pool.profile == profile)
    {
        all.waits.extend(&pool.waits);
        all.never_started += pool.never_started;
    }
    all
}

/// The busy share of `replay`, to a tenth of a percent.
fn share(replay: &Replay) -> String {
    if replay.capacity == 0 {
        return "not known without a budget".to_owned();
    }
    let tenths = replay.charged.saturating_mul(1000) / replay.capacity;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// Byte-milliseconds as GiB-minutes, to a tenth.
fn gib_minutes(amount: u128) -> String {
    let tenths = amount.saturating_mul(10) / (u128::from(1_u64 << 30) * 60_000);
    format!("{}.{}", tenths / 10, tenths % 10)
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

/// A job a replay runs in another profile than its history says: `--move`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Move {
    repository: String,
    job: String,
    profile: String,
}

impl Move {
    /// Moves what `self` names of `jobs`.
    pub(crate) fn apply(&self, jobs: &mut [Ran]) {
        for run in jobs
            .iter_mut()
            .filter(|run| run.repository == self.repository && run.name == self.job)
        {
            run.profile.clone_from(&self.profile);
        }
    }
}

/// Parses `--move owner/repository:job name=profile`: a job's name may hold a colon, a repository's does not, and a
/// profile's holds no `=`.
pub(crate) fn moved(text: &str) -> Result<Move, String> {
    let shape = || format!("{text:?} is not REPOSITORY:JOB=PROFILE");
    let (repository, rest) = text.split_once(':').ok_or_else(shape)?;
    let (job, profile) = rest.rsplit_once('=').ok_or_else(shape)?;
    Ok(Move {
        repository: repository.to_owned(),
        job: job.to_owned(),
        profile: profile.to_owned(),
    })
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
