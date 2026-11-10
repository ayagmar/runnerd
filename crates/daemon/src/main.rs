//! `runnerd`: the daemon and its commands (`init`, `serve`, `stop`, `doctor`, `check-config`). Wires config, GitHub,
//! Docker and the engine together; scheduling lives in `runnerd_core::scheduler`, the API in `api`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use runnerd_core::admission::trace::Trace;
use runnerd_core::admission::{Band, Brake, GrowthCount, GuardHold, HostReserve, pooled};
use runnerd_core::buildcache::BuildCache;
use runnerd_core::cache::Names;
use runnerd_core::config::{ByteSize, Config, parse_age, slice_cpus, slice_memory_max};
use runnerd_core::docker::Docker;
use runnerd_core::github::{self, Github};
use runnerd_core::observe::metrics::Paths;
use runnerd_core::scheduler::engine::{Control, Engine, HostIdentity, Setup, Shutdown};
use runnerd_core::store::{Announced, Store};
use tracing::{info, warn};

mod api;
mod doctor;
mod init;
mod output;
mod simulate;
#[cfg(test)]
mod testdir;

use output::fail;

/// Runs ephemeral GitHub Actions runners in containers within a host budget.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
#[allow(
    clippy::doc_markdown,
    reason = "clap prints these comments as --help, where backticks would show"
)]
enum Command {
    /// Write a starter config sized for this host and a systemd user unit, then print the steps left.
    ///
    /// Never overwrites a file without --force and never runs anything as root: it prints those steps.
    Init {
        /// The repositories to serve, as owner/name.
        #[arg(required = true)]
        repositories: Vec<String>,
        /// The systemd slice runners run in; its MemoryMax is their budget.
        #[arg(long, default_value = "runnerd.slice")]
        slice: String,
        /// Where to write the config [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml].
        #[arg(long)]
        config: Option<PathBuf>,
        /// Replace the config and the unit if they exist.
        #[arg(long)]
        force: bool,
        /// The most memory this host can grow to, such as 24G, for a VM whose hypervisor adds memory as the guest
        /// asks for it. The config gets it as memory_ceiling, and the first runner is sized for the host as it can
        /// grow.
        #[arg(long, value_name = "SIZE")]
        memory_ceiling: Option<String>,
    },
    /// Validate a config file against the slice's memory budget.
    CheckConfig {
        /// The config file [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml].
        path: Option<PathBuf>,
        /// Check against this MemoryMax (such as 33G) instead of reading it from systemd.
        #[arg(long)]
        memory_max: Option<ByteSize>,
    },
    /// Check that the host can run runners, and print the fix for what it is missing.
    Doctor {
        /// Also check this config against the slice it names [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml,
        /// when it exists].
        config: Option<PathBuf>,
    },
    /// Replay the job history through admission by caps, by measured peaks and with a CPU budget, and print what each
    /// would have done.
    ///
    /// Reads the history database read-only; changes nothing.
    Simulate {
        /// The config to replay with [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml].
        #[arg(long)]
        config: Option<PathBuf>,
        /// Replay against this MemoryMax (such as 33G) instead of reading it from systemd.
        #[arg(long)]
        memory_max: Option<ByteSize>,
        /// Replay the jobs queued in the 30m, 12h, 7d, 2w, … up to the history's last job
        #[arg(long, default_value = "30d", value_parser = parse_age)]
        since: Duration,
        /// The slots a profile may use beyond its configured ones, as PROFILE=SLOTS [default: as many as the slice
        /// holds of the profile alone at its cap].
        #[arg(long, value_parser = simulate::slots)]
        slots: Vec<(String, u32)>,
        /// Replay against this many CPUs (such as 32 or 7.5) instead of reading the slice's from systemd.
        #[arg(long, value_parser = simulate::cpus)]
        cpus: Option<u64>,
        /// Replay a CPU budget of the slice's CPUs times this; repeat it to compare factors [default: the config's
        /// `cpu_overcommit`, else 1].
        #[arg(long, value_parser = simulate::overcommit)]
        cpu_overcommit: Vec<f64>,
        /// Also replay the config's policy and slots, `--slots` over them, with no barrier for a starved job, a strict
        /// one and the daemon's, pool by pool and run by run.
        #[arg(long)]
        priority: bool,
        /// Also replay the config's policy and slots as the jobs really ran, each using its recorded samples from its
        /// replayed start, with the plain count and with pooled credits of 4G, 6G and 8G: waits, runs and hazards,
        /// beside what the same jobs waited as they ran.
        #[arg(long)]
        causal: bool,
        /// With --causal, give every run of a job the samples of its heaviest run, placed so that the run reaches its
        /// peak: every cache cold at once.
        #[arg(long, requires = "causal")]
        cold: bool,
        /// With --causal, what the slice holds beside its runners, as the hazards read it (such as 8G) [default: the
        /// p99 of the window's busy moments].
        #[arg(long, requires = "causal")]
        overhead: Option<ByteSize>,
        /// With --causal, what the guards read of that when they decide (such as 1G) [default: --overhead]: a slice
        /// that reads low at a decision and high once the job runs.
        #[arg(long, requires = "causal")]
        guard_overhead: Option<ByteSize>,
        /// With --causal, the credit of a row, 0G for the plain count; repeat it for more rows [default: 0, 4G, 6G, 8G].
        #[arg(long, requires = "causal")]
        credit: Vec<ByteSize>,
        /// With --causal, how rows count what runs: `plain`, by the credits alone, a row for each --credit; `growth`, as
        /// the daemon does (ADR 0130); `top2` or `top3`, a row by what runs uses with room for its two or three
        /// largest growths, at least 6G, for both MemoryMax and the host, and the plain count while the host has under
        /// its margin and a quarter of its memory available, which the options below change. It picks the rows: list
        /// `plain` too to keep the credit rows [default: plain].
        #[arg(long, requires = "causal", value_enum)]
        count: Vec<CountArg>,
        #[command(flatten)]
        rules: Rules,
        /// With --causal, what the host's limit reads of what runs: `top2`, eligible runners at their use with room
        /// for the two largest growths, as the daemon does; or `plain`, every runner's take.
        #[arg(long, requires = "causal", value_enum, default_value_t = HostReserveArg::Top2)]
        host_reserve: HostReserveArg,
        /// With --causal, hold the host's larger margin while any job waits for the host, not only while none fits the
        /// plain margin as the daemon does: what that gives.
        #[arg(long, requires = "causal")]
        any_waiter_band: bool,
        /// With --causal, move the host's other programs this much later, round the window (such as 90m): a desktop's
        /// burst meeting other jobs than it met.
        #[arg(long, requires = "causal", value_parser = parse_age)]
        shift: Option<Duration>,
        /// Replay a job in another profile than the history has, as REPOSITORY:JOB=PROFILE: pools made since it ran.
        #[arg(long = "move", value_parser = simulate::moved)]
        moves: Vec<simulate::Move>,
    },
    /// Run the daemon: schedule runners for queued jobs and serve the API on a Unix socket.
    ///
    /// SIGTERM or SIGINT removes idle runners and exits within 45s, leaving running jobs to the next start, which
    /// adopts them; a second one kills them.
    Serve {
        /// The config file [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml].
        #[arg(long)]
        config: Option<PathBuf>,
        /// The API socket [default: $XDG_RUNTIME_DIR/runnerd.sock].
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Serve the web dashboard on this loopback address; `off` serves none.
        #[arg(long, default_value = runnerd_web::DEFAULT_ADDR)]
        web: String,
    },
    /// Ask the running daemon to stop once its running jobs finish.
    Stop {
        /// Kill running jobs and delete their runners instead of waiting.
        #[arg(long)]
        force: bool,
        /// The API socket [default: $XDG_RUNTIME_DIR/runnerd.sock].
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Some(Command::Init {
            repositories,
            slice,
            config,
            force,
            memory_ceiling,
        }) => init(repositories, slice, config, force, memory_ceiling),
        Some(Command::CheckConfig { path, memory_max }) => {
            config_or_default(path).map(|path| check_config(&path, memory_max))
        }
        Some(Command::Doctor { config }) => Ok(doctor(
            config
                .or_else(|| default_config().filter(|path| path.exists()))
                .as_deref(),
        )),
        Some(Command::Simulate {
            config,
            memory_max,
            since,
            slots,
            cpus,
            cpu_overcommit,
            priority,
            causal,
            cold,
            overhead,
            guard_overhead,
            credit,
            count,
            rules,
            host_reserve,
            any_waiter_band,
            shift,
            moves,
        }) => config_or_default(config).and_then(|path| {
            let rows = causal
                .then(|| rows(&credit, &count, rules.variant()))
                .transpose()?
                .unwrap_or_default();
            run_simulation(
                &path,
                Limits {
                    memory_max,
                    cpus,
                    cpu_overcommit,
                    causal: causal.then_some(Causal {
                        cold,
                        overhead,
                        guard_overhead,
                        rows,
                        host_reserve: match host_reserve {
                            HostReserveArg::Plain => HostReserve::Plain,
                            HostReserveArg::Top2 => HostReserve::Top2,
                        },
                        band: if any_waiter_band {
                            Band::AnyWaiter
                        } else {
                            Band::NoWaiterFits
                        },
                        shift,
                    }),
                    priority: if priority {
                        simulate::Comparing::Priority
                    } else {
                        simulate::Comparing::Policies
                    },
                    moves,
                },
                since,
                &slots.into_iter().collect(),
            )
        }),
        Some(Command::Serve {
            config,
            socket,
            web,
        }) => runtime().and_then(|runtime| {
            let served = runtime.block_on(serve(config, socket, &web));
            // A blocking task still running, such as a doctor's check of a Docker that hangs, does not hold the exit.
            runtime.shutdown_timeout(EXIT_GRACE);
            served
        }),
        Some(Command::Stop { force, socket }) => {
            runtime().and_then(|runtime| runtime.block_on(stop(force, socket)))
        }
        None => Ok(ExitCode::SUCCESS),
    };
    result.unwrap_or_else(|code| code)
}

fn init(
    repositories: Vec<String>,
    slice: String,
    config: Option<PathBuf>,
    force: bool,
    memory_ceiling: Option<String>,
) -> Result<ExitCode, ExitCode> {
    let config = config_or_default(config)?;
    let config = std::path::absolute(&config).map_err(|error| {
        fail(&format_args!(
            "cannot resolve {}: {error}",
            config.display()
        ))
    })?;
    let unit = config_dir()
        .map(|dir| dir.join("systemd/user/runnerd.service"))
        .ok_or_else(|| fail(&"neither XDG_CONFIG_HOME nor HOME is set"))?;
    let runnerd = std::env::current_exe().map_err(|error| fail(&error))?;
    Ok(init::run(&init::Request {
        repositories,
        slice,
        runnerd,
        config,
        unit,
        overwrite: if force {
            init::Overwrite::Replace
        } else {
            init::Overwrite::Refuse
        },
        memory_ceiling,
    }))
}

fn check_config(path: &Path, memory_max: Option<ByteSize>) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => return fail(&format_args!("cannot read {}: {error}", path.display())),
    };
    let (_, check) = Config::check(&text, |slice| match memory_max {
        Some(memory_max) => Ok(Some(memory_max)),
        None => slice_memory_max(slice),
    });
    if !check.valid {
        return fail(&check.message);
    }
    for warning in &check.warnings {
        output::warning(warning);
    }
    output::line(format_args!("{}: {}", path.display(), check.message));
    ExitCode::SUCCESS
}

/// What `runnerd simulate` replays against, where given instead of read from the slice and the config.
struct Limits {
    memory_max: Option<ByteSize>,
    /// Millicores.
    cpus: Option<u64>,
    cpu_overcommit: Vec<f64>,
    priority: simulate::Comparing,
    causal: Option<Causal>,
    moves: Vec<simulate::Move>,
}

/// `runnerd simulate --causal --count`: how a row counts what runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CountArg {
    /// Every runner's take, less the credits.
    Plain,
    /// As the daemon does: the growth count with its brake and every rule (ADR 0130).
    Growth,
    /// What runs uses, with room for its two largest growths.
    Top2,
    /// What runs uses, with room for its three largest growths.
    Top3,
}

/// The flags of `runnerd simulate --causal` that give the top2 and top3 rows the growth count's rules (ADR 0130).
#[derive(Debug, Clone, Copy, clap::Args)]
#[allow(
    clippy::doc_markdown,
    reason = "clap prints these comments as --help, where backticks would show"
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per rule, as the command line has them"
)]
struct Rules {
    /// With --causal, give the top2 and top3 rows the growth brake: their count rests for 30 minutes once what
    /// runs grew past the room it kept within 10 minutes.
    #[arg(long, requires = "causal")]
    brake: bool,
    /// With --causal, bound the top2 and top3 rows' credit as the daemon's is: MemoryMax's at most the slice's
    /// room under its high-water mark less 2G, and both at most what the host has past what it keeps.
    #[arg(long, requires = "causal")]
    bound: bool,
    /// With --causal, make the top2 and top3 rows keep room for at least every young runner's growth: one whose
    /// job has run under a minute, or that has no job yet.
    #[arg(long, requires = "causal")]
    young: bool,
    /// With --causal, count the top2 and top3 rows by the credits, not the plain count, below the guard's line
    /// and while the brake holds.
    #[arg(long, requires = "causal")]
    credits_below: bool,
    /// With --causal and --brake, rest the top2 and top3 rows' count for 5 minutes from a plan that finds it below the
    /// guard's line, and again from the first plan after that still below it.
    #[arg(long, requires = "brake")]
    guard_hold: bool,
    /// With --causal, keep the top2 and top3 rows' credits, MemoryMax's and the host's, at most this.
    #[arg(long, requires = "causal")]
    ceiling: Option<ByteSize>,
}

impl Rules {
    /// The rules the flags ask for.
    fn variant(self) -> Variant {
        Variant {
            brake: if self.brake { Brake::On } else { Brake::Off },
            bound: if self.bound {
                pooled::Bound::HighWater
            } else {
                pooled::Bound::Open
            },
            young: if self.young {
                pooled::Young::Covered
            } else {
                pooled::Young::Ranked
            },
            below: if self.credits_below {
                pooled::Below::Credits
            } else {
                pooled::Below::Plain
            },
            guard: if self.guard_hold {
                GuardHold::On
            } else {
                GuardHold::Off
            },
            ceiling: self.ceiling.map(|ceiling| ceiling.0),
        }
    }
}

/// The rules `--brake`, `--bound`, `--young`, `--credits-below`, `--guard-hold` and `--ceiling` give the top2 and top3
/// rows.
#[derive(Debug, Clone, Copy)]
struct Variant {
    brake: Brake,
    bound: pooled::Bound,
    young: pooled::Young,
    below: pooled::Below,
    guard: GuardHold,
    ceiling: Option<u64>,
}

/// The rows `--credit`, `--count` and the rules of `variant` ask `runnerd simulate --causal` for: each `plain` a row for
/// each credit, [`CREDITS`] without one; each growth count a row.
fn rows(
    credits: &[ByteSize],
    counts: &[CountArg],
    variant: Variant,
) -> Result<Vec<simulate::Counting>, ExitCode> {
    if !credits.is_empty() && !counts.is_empty() && !counts.contains(&CountArg::Plain) {
        return Err(fail(&"--credit needs --count plain beside a growth count"));
    }
    let counts = if counts.is_empty() {
        &[CountArg::Plain][..]
    } else {
        counts
    };
    let mut rows = Vec::new();
    for count in counts {
        match count {
            CountArg::Plain if credits.is_empty() => {
                rows.extend(CREDITS.map(simulate::Counting::Credit));
            }
            CountArg::Plain => rows.extend(
                credits
                    .iter()
                    .map(|credit| simulate::Counting::Credit(credit.0)),
            ),
            CountArg::Growth => rows.push(simulate::Counting::Growth(GrowthCount::DAEMON)),
            CountArg::Top2 | CountArg::Top3 => {
                rows.push(simulate::Counting::Growth(GrowthCount {
                    growth: pooled::Growth {
                        bound: variant.bound,
                        young: variant.young,
                        below: variant.below,
                        ceiling: variant.ceiling,
                        ..pooled::Growth::top(if *count == CountArg::Top2 { 2 } else { 3 })
                    },
                    brake: variant.brake,
                    guard: variant.guard,
                }));
            }
        }
    }
    let varies = variant.brake == Brake::On
        || variant.bound == pooled::Bound::HighWater
        || variant.young == pooled::Young::Covered
        || variant.below == pooled::Below::Credits
        || variant.ceiling.is_some();
    let tops = counts
        .iter()
        .any(|count| matches!(count, CountArg::Top2 | CountArg::Top3));
    if varies && !tops {
        return Err(fail(
            &"--brake, --bound, --young, --credits-below and --ceiling need --count top2 or top3",
        ));
    }
    Ok(rows)
}

/// `runnerd simulate --causal --host-reserve`: what the host's limit reads of what runs (ADR 0108).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum HostReserveArg {
    /// Every runner's take.
    Plain,
    /// Eligible runners at their use, with room for the two largest growths.
    Top2,
}

/// How `runnerd simulate --causal` replays.
#[derive(Debug, Clone)]
struct Causal {
    /// Every run takes its job's heaviest samples.
    cold: bool,
    /// What the slice holds beside its runners, where given.
    overhead: Option<ByteSize>,
    /// What the guards read of that, where given.
    guard_overhead: Option<ByteSize>,
    /// How each row counts.
    rows: Vec<simulate::Counting>,
    /// What the host's limit reads of what runs.
    host_reserve: HostReserve,
    /// What holds up the host's larger margin.
    band: Band,
    /// How much later the host's other programs run, where given.
    shift: Option<Duration>,
}

/// The most credit of each row `runnerd simulate --causal` replays by default: the plain count, then the pooled one
/// around its constant.
const CREDITS: [u64; 4] = [0, 4 << 30, 6 << 30, 8 << 30];

/// How far before a replay's window it reads what the daemon would know of the jobs by then, in ms: as far as the
/// daemon reads back.
const KNOWN_BEFORE: u64 = 30 * 24 * 3600 * 1000;

fn run_simulation(
    path: &Path,
    limits: Limits,
    since: Duration,
    slots: &std::collections::BTreeMap<String, u32>,
) -> Result<ExitCode, ExitCode> {
    let config = Config::load(path).map_err(|error| fail(&error))?;
    let cpus = match limits.cpus {
        Some(cpus) => cpus,
        None => slice_cpus(&config.host.slice).map_err(|error| {
            fail(&format_args!(
                "cannot read {}'s CPUs: {error}; pass --cpus",
                config.host.slice
            ))
        })?,
    };
    let overcommit = if limits.cpu_overcommit.is_empty() {
        vec![config.host.cpu_overcommit.unwrap_or(1.0)]
    } else {
        limits.cpu_overcommit
    };
    let memory_max = match limits.memory_max {
        Some(max) => Some(max),
        None => slice_memory_max(&config.host.slice).map_err(|error| {
            fail(&format_args!(
                "cannot read {}'s MemoryMax: {error}; pass --memory-max",
                config.host.slice
            ))
        })?,
    };
    let database = config
        .history
        .database
        .clone()
        .or_else(default_database)
        .ok_or_else(|| fail(&"neither XDG_STATE_HOME nor HOME is set"))?;
    let last = runnerd_core::store::last_queued_at(&database)
        .map_err(|error| fail(&error))?
        .unwrap_or_default();
    let since = last.saturating_sub(u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
    let mut history = runnerd_core::store::sizing_at(&database, since)
        .map_err(|error| fail(&error))?
        .ok_or_else(|| {
            fail(&format_args!(
                "there is no job history at {}",
                database.display()
            ))
        })?;
    limits
        .moves
        .iter()
        .for_each(|moved| moved.apply(&mut history.jobs));
    if limits.causal.is_none() {
        output::line(simulate::report(
            &config,
            memory_max.map(|max| max.0),
            &simulate::Cpus {
                millicores: cpus,
                overcommit: &overcommit,
            },
            &history.jobs,
            slots,
        ));
    }
    let compares = limits.priority == simulate::Comparing::Priority || limits.causal.is_some();
    if !compares {
        return Ok(ExitCode::SUCCESS);
    }
    let mut known = runnerd_core::store::sizing_at(&database, since.saturating_sub(KNOWN_BEFORE))
        .map_err(|error| fail(&error))?
        .unwrap_or_default();
    limits
        .moves
        .iter()
        .for_each(|moved| moved.apply(&mut known.jobs));
    let comparison = simulate::Comparison {
        memory_max: memory_max.map(|max| max.0),
        cpu_budget: config
            .host
            .cpu_overcommit
            .map(|factor| runnerd_core::config::cpu_budget(cpus, factor)),
        jobs: &history.jobs,
        known: &known,
        slots,
        left_out: history.left_out,
    };
    if limits.priority == simulate::Comparing::Priority {
        output::line(simulate::priority(&config, &comparison));
    }
    if let Some(causal) = &limits.causal {
        let trace = trace_of(&config, &database, since, causal, &history.jobs)?;
        let counts = simulate::Counts {
            rows: &causal.rows,
            host_reserve: causal.host_reserve,
            band: causal.band,
            barriers: limits.priority,
        };
        output::line(simulate::causal(&config, &comparison, &trace, &counts));
    }
    Ok(ExitCode::SUCCESS)
}

/// How the jobs from `since` (ms) of the history at `database` ran, as `causal` asks.
fn trace_of(
    config: &Config,
    database: &Path,
    since: u64,
    causal: &Causal,
    jobs: &[runnerd_core::sizing::Ran],
) -> Result<Trace, ExitCode> {
    let poll = u64::try_from(config.host.poll_interval.as_millis()).unwrap_or(u64::MAX);
    let overhead = causal.overhead.map(|overhead| overhead.0);
    let mut trace = runnerd_core::store::trace_at(database, since, poll, overhead)
        .map_err(|error| fail(&error))?
        .unwrap_or_default();
    if let Some(guard) = causal.guard_overhead {
        trace.guard_overhead = guard.0;
    }
    if let Some(shift) = causal.shift {
        trace = trace.shifted(u64::try_from(shift.as_millis()).unwrap_or(u64::MAX));
    }
    Ok(if causal.cold { trace.cold(jobs) } else { trace })
}

fn doctor(path: Option<&Path>) -> ExitCode {
    let config = match path.map(Config::load).transpose() {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let mut host = doctor::Host::read(config.as_ref(), path);
    if let Some(config) = &config
        && let Some(database) = config.history.database.clone().or_else(default_database)
    {
        let memory_max = match &host.slice {
            Some(Ok(slice)) => slice.memory_max.map(|max| max.0),
            _ => None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        host.sizing = doctor::read_sizing(config, &database, memory_max, now);
    }
    let checks = doctor::checks(&host, config.as_ref());
    for check in &checks {
        output::line(check);
    }
    if runnerd_core::api::Doctor::all_passed(&checks) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Worker threads at most. The daemon serves a few requests a second; on a many-core host one worker per core
/// spends more CPU contending for the allocator (musl's especially) than it saves: 50 event subscribers through 200
/// pause and resume pairs took 16.5 s of CPU on 32 workers and 1.2 s on 4.
const WORKERS: usize = 4;

/// How long API and dashboard requests still open get to finish once the engine stopped (ADR 0091).
const DRAIN: Duration = Duration::from_secs(10);

/// How long blocking tasks still running get once the daemon is done, before it exits without them (ADR 0091).
const EXIT_GRACE: Duration = Duration::from_secs(5);

fn runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores.min(WORKERS))
        .enable_all()
        .build()
        .map_err(|error| fail(&error))
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
}

fn default_config() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("runnerd/runnerd.toml"))
}

fn config_or_default(config: Option<PathBuf>) -> Result<PathBuf, ExitCode> {
    config
        .or_else(default_config)
        .ok_or_else(|| fail(&"neither XDG_CONFIG_HOME nor HOME is set; name the config file"))
}

/// `$XDG_STATE_HOME/runnerd/runnerd.db`, or `~/.local/state/runnerd/runnerd.db`.
/// What the engine starts from in the job history.
struct History {
    store: Store,
    /// The profiles and repositories its events last announced.
    announced: Announced,
    /// The repositories recorded paused.
    paused: Vec<String>,
    /// The dashboard's session secret, kept beside it.
    sessions: std::sync::Arc<runnerd_web::Sessions>,
}

/// The job history, the profiles and repositories its events last announced and the ones recorded paused, and the
/// dashboard's session secret beside it.
fn open_history(config: &Config) -> Result<History, ExitCode> {
    let database = config
        .history
        .database
        .clone()
        .or_else(default_database)
        .ok_or_else(|| fail(&"neither XDG_STATE_HOME nor HOME is set; set history.database"))?;
    let store = Store::open(&database).map_err(|error| {
        fail(&format_args!(
            "cannot open the job history {}: {error}",
            database.display()
        ))
    })?;
    info!("job history in {}", database.display());
    let announced = store.announced().map_err(|error| {
        fail(&format_args!(
            "cannot read the profiles and repositories in the job history {}: {error}",
            database.display()
        ))
    })?;
    let paused = store.paused_repositories().map_err(|error| {
        fail(&format_args!(
            "cannot read the paused repositories in the job history {}: {error}",
            database.display()
        ))
    })?;
    let session_file = runnerd_web::session_file(&database);
    let sessions = runnerd_web::Sessions::open(session_file.clone()).map_err(|error| {
        fail(&format_args!(
            "cannot read or write the dashboard's session secret {}: {error}",
            session_file.display()
        ))
    })?;
    Ok(History {
        store,
        announced,
        paused,
        sessions,
    })
}

/// The shared build cache: its entries in `[caches.build] directory` or `$XDG_CACHE_HOME/runnerd/build-cache`, each
/// runner's socket beside the API's. Opened whether `[caches.build]` is on or not, so a reload can turn it on.
/// Without it runners build without it, which the log says.
fn open_build_cache(config: &Config, socket: &Path) -> Option<BuildCache> {
    let directory = config.caches.build.directory.clone().or_else(|| {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
            .map(|dir| dir.join("runnerd/build-cache"))
    });
    let Some(directory) = directory else {
        warn!("no build cache: neither XDG_CACHE_HOME nor HOME is set; set caches.build.directory");
        return None;
    };
    // Absolute, as Docker mounts each runner's directory from it and refuses a relative path.
    let sockets = match std::path::absolute(socket.with_file_name("runnerd-build-cache")) {
        Ok(sockets) => sockets,
        Err(error) => {
            warn!("no build cache: cannot resolve the socket's directory: {error}");
            return None;
        }
    };
    match BuildCache::open(&directory, &sockets, config.caches.build.max_size.0) {
        Ok(cache) => {
            let size = cache.size();
            info!(
                "build cache in {}: {} entries, {}",
                directory.display(),
                size.entries,
                ByteSize(size.bytes)
            );
            Some(cache)
        }
        Err(error) => {
            warn!(
                "no build cache: cannot open {}: {error}",
                directory.display()
            );
            None
        }
    }
}

fn default_database() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))?;
    Some(dir.join("runnerd/runnerd.db"))
}

fn socket_or_default(socket: Option<PathBuf>) -> Result<PathBuf, ExitCode> {
    socket
        .or_else(api::default_socket)
        .ok_or_else(|| fail(&"XDG_RUNTIME_DIR is not set; pass --socket"))
}

async fn serve(
    config: Option<PathBuf>,
    socket: Option<PathBuf>,
    web: &str,
) -> Result<ExitCode, ExitCode> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let path = config_or_default(config)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|error| fail(&format_args!("cannot read {}: {error}", path.display())))?;
    let config = Config::parse(&text).map_err(|error| fail(&error))?;
    let memory_max = slice_memory_max(&config.host.slice).map_err(|error| fail(&error))?;
    // Only a CPU budget needs the slice's CPUs: without one, a slice systemd cannot describe still runs, and the
    // CPUs read only scale the dashboard's CPU chart.
    let cpus = if config.host.cpu_overcommit.is_some() {
        Some(slice_cpus(&config.host.slice).map_err(|error| fail(&error))?)
    } else {
        slice_cpus(&config.host.slice).ok()
    };
    for warning in config.validate(memory_max).map_err(|error| fail(&error))? {
        warn!("{warning}");
    }
    // Read once now, so a missing login fails here rather than at the first poll.
    github::gh_token().map_err(|error| fail(&error))?;
    let github =
        Github::new(github::API_URL, Box::new(github::gh_token)).map_err(|error| fail(&error))?;
    let docker = Docker::connect().map_err(|error| fail(&error))?;
    docker.info().await.map_err(|error| fail(&error))?;
    let identity = HostIdentity::read().map_err(|error| fail(&error))?;
    let History {
        store,
        announced,
        paused,
        sessions,
    } = open_history(&config)?;
    let socket = socket_or_default(socket)?;
    let listener = api::bind(&socket).await.map_err(|error| fail(&error))?;
    let web_listener = runnerd_web::listen(web)
        .await
        .map_err(|error| fail(&format_args!("--web {web}: {error}")))?;
    let proxies = config.web.origins.clone();
    let build_cache = open_build_cache(&config, &socket);
    let (engine, handle) = Engine::new(
        config,
        github,
        docker,
        Setup {
            names: Names::new("runnerd"),
            identity,
            memory_max: memory_max.map(|max| max.0),
            cpus,
            store: store.clone(),
            announced,
            paused,
            paths: Paths::default(),
            build_cache,
        },
    );
    tokio::spawn(shut_down_on_signals(handle.control.clone()));
    info!("serving the API on {}", socket.display());
    let engine = tokio::spawn(engine.run());
    let (stopped, stop) = tokio::sync::oneshot::channel::<()>();
    let config = api::ConfigSource {
        path,
        text,
        memory_max: slice_memory_max,
        cpus: slice_cpus,
        sync_dir: api::sync_dir,
        auto_every: api::AUTO_SIZING_EVERY,
        notify: api::NOTIFY,
    };
    let server = tokio::spawn(api::serve(
        listener,
        handle,
        config,
        sessions.clone(),
        async {
            let _ = stop.await;
        },
    ));
    let (web_stopped, web_stop) = tokio::sync::oneshot::channel::<()>();
    let web = web_listener
        .map(|listener| runnerd_web::spawn(listener, socket.clone(), proxies, sessions, web_stop));
    let engine_done = engine.await;
    let _ = web_stopped.send(());
    let _ = stopped.send(());
    store.close();
    store.flush().await;
    let drained = drain(server, web).await;
    let _ = std::fs::remove_file(&socket);
    engine_done.map_err(|error| fail(&error))?;
    drained?;
    Ok(ExitCode::SUCCESS)
}

/// Waits up to `DRAIN` for the API and the dashboard to finish the requests still open; past it, stops without
/// them.
async fn drain(
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    web: Option<tokio::task::JoinHandle<()>>,
) -> Result<(), ExitCode> {
    let drained = tokio::time::timeout(DRAIN, async {
        let server_done = server.await;
        if let Some(web) = web {
            let _ = web.await;
        }
        server_done
    })
    .await;
    let Ok(server_done) = drained else {
        warn!(
            "stopping with API requests still open after {}s",
            DRAIN.as_secs()
        );
        return Ok(());
    };
    server_done
        .map_err(|error| fail(&error))?
        .map_err(|error| fail(&error))
}

/// Stops the engine on SIGTERM or SIGINT, leaving running jobs to the next start, and kills them on a second.
async fn shut_down_on_signals(control: std::sync::Arc<Control>) {
    for mode in [Shutdown::Detach, Shutdown::Force] {
        if wait_for_signal().await.is_err() {
            return;
        }
        if mode == Shutdown::Force {
            info!("killing running jobs");
        } else {
            info!("stopping, leaving running jobs to the next start; signal again to kill them");
        }
        control.shutdown(mode);
    }
}

/// Waits for SIGTERM or SIGINT.
async fn wait_for_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

async fn stop(force: bool, socket: Option<PathBuf>) -> Result<ExitCode, ExitCode> {
    let socket = socket_or_default(socket)?;
    github::install_crypto_provider();
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .map_err(|error| fail(&error))?;
    let body = serde_json::json!({ "force": force }).to_string();
    let response = client
        .post("http://runnerd/v1/shutdown")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| fail(&error))?;
    if !response.status().is_success() {
        return Err(fail(&format!("the daemon answered {}", response.status())));
    }
    // How it stops now, which an earlier stop may have made more urgent; a daemon before the answer says nothing.
    let answer = response
        .text()
        .await
        .ok()
        .and_then(|body| serde_json::from_str::<runnerd_core::api::ShutdownAnswer>(&body).ok());
    output::line(format_args!(
        "runnerd is {}",
        answer.map_or("shutting down", |answer| answer.stopping.line())
    ));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemons_growth_count_is_top3_with_every_rule() {
        // Catches `--count growth` drifting from the rules `simulate` names it by, so that the table's row for the
        // daemon's count replays another one.
        let every = Variant {
            brake: Brake::On,
            bound: pooled::Bound::HighWater,
            young: pooled::Young::Covered,
            below: pooled::Below::Credits,
            guard: GuardHold::On,
            ceiling: pooled::GROWTH.ceiling,
        };
        let none = Variant {
            brake: Brake::Off,
            bound: pooled::Bound::Open,
            young: pooled::Young::Ranked,
            below: pooled::Below::Plain,
            guard: GuardHold::Off,
            ceiling: None,
        };
        assert_eq!(
            rows(&[], &[CountArg::Top3], every).ok(),
            rows(&[], &[CountArg::Growth], none).ok()
        );
        assert!(
            rows(&[], &[CountArg::Growth], every).is_err(),
            "a rule without a top2 or top3 row is refused"
        );
    }
}
