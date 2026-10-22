//! `runnerd`: the daemon and its commands (`serve`, `stop`, `doctor`, `check-config`). Wires config, GitHub,
//! Docker and the engine together; scheduling lives in `runnerd_core::scheduler`, the API in `api`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use runnerd_core::cache::Names;
use runnerd_core::config::{ByteSize, Config, slice_memory_max};
use runnerd_core::docker::Docker;
use runnerd_core::github::{self, Github};
use runnerd_core::observe::metrics::Paths;
use runnerd_core::scheduler::engine::{Engine, HostIdentity, Setup};
use runnerd_core::store::Store;
use tracing::{info, warn};

mod api;
mod doctor;
mod output;

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
    /// Validate a config file against the slice's memory budget.
    CheckConfig {
        path: PathBuf,
        /// Check against this MemoryMax (such as 33G) instead of reading it from systemd.
        #[arg(long)]
        memory_max: Option<ByteSize>,
    },
    /// Check that the host can run runners, and print the fix for what it is missing.
    Doctor {
        /// Also check this config against the slice it names.
        config: Option<PathBuf>,
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
        Some(Command::CheckConfig { path, memory_max }) => Ok(check_config(&path, memory_max)),
        Some(Command::Doctor { config }) => Ok(doctor(config.as_deref())),
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

fn doctor(path: Option<&Path>) -> ExitCode {
    let config = match path.map(Config::load).transpose() {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let host = doctor::Host::read(config.as_ref());
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

fn runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Runtime::new().map_err(|error| fail(&error))
}

fn default_config() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
    Some(dir.join("runnerd/runnerd.toml"))
}

/// `$XDG_STATE_HOME/runnerd/runnerd.db`, or `~/.local/state/runnerd/runnerd.db`.
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
    let path = config
        .or_else(default_config)
        .ok_or_else(|| fail(&"neither XDG_CONFIG_HOME nor HOME is set; pass --config"))?;
    let text = std::fs::read_to_string(&path)
        .map_err(|error| fail(&format_args!("cannot read {}: {error}", path.display())))?;
    let config = Config::parse(&text).map_err(|error| fail(&error))?;
    let memory_max = slice_memory_max(&config.host.slice).map_err(|error| fail(&error))?;
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
            store: store.clone(),
            paths: Paths::default(),
        },
    );
    let control = handle.control.clone();
    tokio::spawn(async move {
        for force in [false, true] {
            if wait_for_signal().await.is_err() {
                return;
            }
            info!(
                "{}",
                if force {
                    "killing running jobs"
                } else {
                    "stopping once running jobs finish; signal again to kill them"
                }
            );
            control.shutdown(force);
        }
    });
    info!("serving the API on {}", socket.display());
    let engine = tokio::spawn(engine.run());
    let (stopped, stop) = tokio::sync::oneshot::channel::<()>();
    let config = api::ConfigSource {
        path,
        text,
        memory_max: slice_memory_max,
        sync_dir: api::sync_dir,
    };
    let server = tokio::spawn(api::serve(listener, handle, config, async {
        let _ = stop.await;
    }));
    let (web_stopped, web_stop) = tokio::sync::oneshot::channel::<()>();
    let web = web_listener.map(|listener| runnerd_web::spawn(listener, socket.clone(), web_stop));
    let engine_done = engine.await;
    let _ = web_stopped.send(());
    let _ = stopped.send(());
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
