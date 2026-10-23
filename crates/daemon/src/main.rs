//! `runnerd`: the daemon and its commands (`init`, `serve`, `stop`, `doctor`, `check-config`). Wires config, GitHub,
//! Docker and the engine together; scheduling lives in `runnerd_core::scheduler`, the API in `api`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use runnerd_core::cache::Names;
use runnerd_core::config::{ByteSize, Config, parse_age, slice_cpus, slice_memory_max};
use runnerd_core::docker::Docker;
use runnerd_core::github::{self, Github};
use runnerd_core::observe::metrics::Paths;
use runnerd_core::scheduler::engine::{Engine, HostIdentity, RunningJobs, Setup};
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
        /// Replay the jobs queued in the last 30m, 12h, 7d, 2w, …
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
    },
    /// Run the daemon: schedule runners for queued jobs and serve the API on a Unix socket.
    ///
    /// SIGTERM or SIGINT stops polling and waits for running jobs; a second one kills them.
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
        }) => init(repositories, slice, config, force),
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
        }) => config_or_default(config).and_then(|path| {
            run_simulation(
                &path,
                Limits {
                    memory_max,
                    cpus,
                    cpu_overcommit,
                },
                since,
                &slots.into_iter().collect(),
            )
        }),
        Some(Command::Serve {
            config,
            socket,
            web,
        }) => runtime().and_then(|runtime| runtime.block_on(serve(config, socket, &web))),
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
}

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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let since = u64::try_from(now)
        .unwrap_or(u64::MAX)
        .saturating_sub(u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
    let history = runnerd_core::store::sizing_at(&database, since)
        .map_err(|error| fail(&error))?
        .ok_or_else(|| {
            fail(&format_args!(
                "there is no job history at {}",
                database.display()
            ))
        })?;
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
    Ok(ExitCode::SUCCESS)
}

fn doctor(path: Option<&Path>) -> ExitCode {
    let config = match path.map(Config::load).transpose() {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let mut host = doctor::Host::read(config.as_ref());
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
    if checks
        .iter()
        .any(|check| check.status == runnerd_core::api::CheckStatus::Fail)
    {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Worker threads at most. The daemon serves a few requests a second; on a many-core host one worker per core
/// spends more CPU contending for the allocator (musl's especially) than it saves: 50 event subscribers through 200
/// pause and resume pairs took 16.5 s of CPU on 32 workers and 1.2 s on 4.
const WORKERS: usize = 4;

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
}

/// The job history, the profiles and repositories its events last announced and the ones recorded paused.
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
    Ok(History {
        store,
        announced,
        paused,
    })
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
    // Only a CPU budget needs the slice's CPUs: without one, a slice systemd cannot describe still runs.
    let cpus = if config.host.cpu_overcommit.is_some() {
        Some(slice_cpus(&config.host.slice).map_err(|error| fail(&error))?)
    } else {
        None
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
    } = open_history(&config)?;
    let socket = socket_or_default(socket)?;
    let listener = api::bind(&socket).await.map_err(|error| fail(&error))?;
    let web_listener = runnerd_web::listen(web)
        .await
        .map_err(|error| fail(&format_args!("--web {web}: {error}")))?;
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
        },
    );
    let control = handle.control.clone();
    tokio::spawn(async move {
        for jobs in [RunningJobs::Keep, RunningJobs::Kill] {
            if wait_for_signal().await.is_err() {
                return;
            }
            info!(
                "{}",
                if jobs == RunningJobs::Kill {
                    "killing running jobs"
                } else {
                    "stopping once running jobs finish; signal again to kill them"
                }
            );
            control.shutdown(jobs);
        }
    });
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
    };
    let server = tokio::spawn(api::serve(listener, handle, config, async {
        let _ = stop.await;
    }));
    let (web_stopped, web_stop) = tokio::sync::oneshot::channel::<()>();
    let web = web_listener.map(|listener| runnerd_web::spawn(listener, socket.clone(), web_stop));
    let engine_done = engine.await;
    let _ = web_stopped.send(());
    let _ = stopped.send(());
    store.close();
    store.flush().await;
    let server_done = server.await;
    if let Some(web) = web {
        let _ = web.await;
    }
    let _ = std::fs::remove_file(&socket);
    engine_done.map_err(|error| fail(&error))?;
    server_done
        .map_err(|error| fail(&error))?
        .map_err(|error| fail(&error))?;
    Ok(ExitCode::SUCCESS)
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
    output::line(format_args!(
        "runnerd stops {}",
        if force {
            "now, killing running jobs"
        } else {
            "once running jobs finish"
        }
    ));
    Ok(ExitCode::SUCCESS)
}
