//! `runnerctl`: the command-line client. It talks to the daemon's API only, never to Docker or GitHub. Commands map
//! to API calls here; the transport is `client`, text for people `human`, the live view `watch`, printing `output`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use runnerd_core::api::{
    Caches, CheckStatus, ConfigCheck, ConfigFile, Doctor, Event, EventQuery, Job, JobPage,
    JobQuery, JobState, LogQuery, PauseQuery, Paused, Status,
};
use serde::Serialize;

mod client;
mod human;
mod output;
#[cfg(test)]
mod testing;
mod watch;

use client::{Client, ClientError};
use output::fail;

/// Command-line client for the runnerd daemon.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The daemon's API socket [default: `$XDG_RUNTIME_DIR/runnerd.sock`].
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print the API's JSON instead of text: the response type unchanged, one event per line for `watch`.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(
    clippy::doc_markdown,
    reason = "clap prints these comments as --help, where backticks would show"
)]
enum Command {
    /// Show the memory budget, every slot and what runs in it, and the repositories with their queues.
    Status,
    /// List jobs, newest first.
    Jobs {
        /// Only this repository (owner/name).
        #[arg(long)]
        repo: Option<String>,
        /// Only jobs that concluded failure; same as --conclusion failure.
        #[arg(long, conflicts_with = "conclusion")]
        failed: bool,
        /// Only jobs with this conclusion, such as success, failure or cancelled.
        #[arg(long)]
        conclusion: Option<String>,
        /// Only jobs in this state: queued, running, completed, lost or elsewhere.
        #[arg(long, value_parser = parse_state)]
        state: Option<JobState>,
        /// Only jobs queued in the last 30m, 12h, 7d, 2w, ….
        #[arg(long, value_parser = parse_age)]
        since: Option<u64>,
        /// Only jobs older than this job ID: the next page.
        #[arg(long)]
        before: Option<u64>,
        /// At most this many jobs [default: 50, at most 500].
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Show one job: its timeline, steps with their durations, CPU, memory and network.
    Job { id: u64 },
    /// Print a job's log.
    Logs {
        id: u64,
        /// Keep printing lines as they arrive, until the job's record is final.
        #[arg(short, long)]
        follow: bool,
        /// Start at this line, 0-based.
        #[arg(long)]
        from: Option<u64>,
    },
    /// Live view of slots, running jobs and their steps, queued and finished jobs.
    ///
    /// Redraws in place on a terminal; otherwise prints one line per event.
    Watch,
    /// Give a repository no new runners. Refused while it runs a job, unless --force.
    Pause {
        /// owner/name
        repository: String,
        /// Cancel its running jobs' workflow runs and remove their runners.
        #[arg(long)]
        force: bool,
    },
    /// Let a paused repository get runners again.
    Resume {
        /// owner/name
        repository: String,
    },
    /// Make a profile's slot 1 layers its new snapshot, once its slots are free; waits until it is done.
    Reseed {
        profile: String,
        /// Return once the daemon accepted the request.
        #[arg(long)]
        no_wait: bool,
    },
    /// Show the cache volumes, their sizes and limits.
    Caches {
        /// Empty this volume (a slot layer or a shared cache) instead; refused while a runner uses it.
        #[arg(long, value_name = "VOLUME")]
        clean: Option<String>,
    },
    /// Show or check the daemon's config.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Check that the host can run runners. Runs `runnerd doctor` here when the daemon is not running.
    Doctor,
    /// Open the web dashboard, logging in with the daemon's one-time URL.
    Web {
        /// Print the URL instead of opening it.
        #[arg(long)]
        no_open: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the config the daemon runs with, and its path.
    Show,
    /// Check a config file against the daemon's budget, as `runnerd check-config` does; without a file, the
    /// daemon's own config.
    Check { file: Option<PathBuf> },
}

/// Why a command failed.
#[derive(Debug)]
enum Error {
    /// The daemon could not be asked, or refused.
    Client(ClientError),
    /// Something here: an argument, a file, a program to run.
    Local(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Client(error) => error.fmt(f),
            Error::Local(error) => f.write_str(error),
        }
    }
}

impl From<ClientError> for Error {
    fn from(error: ClientError) -> Error {
        Error::Client(error)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(socket) = cli.socket.clone().or_else(default_socket) else {
        return fail(&"XDG_RUNTIME_DIR is not set; pass --socket");
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error),
    };
    match runtime.block_on(run(&Client::new(socket), cli.json, cli.command)) {
        Ok(code) => code,
        Err(error) => fail(&error),
    }
}

/// `$XDG_RUNTIME_DIR/runnerd.sock`, where `runnerd serve` listens by default.
fn default_socket() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| Path::new(&dir).join("runnerd.sock"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_state(text: &str) -> Result<JobState, String> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .map_err(|_| "expected queued, running, completed, lost or elsewhere".to_owned())
}

/// `30m`, `12h`, `7d`, `2w` as the time that long ago.
fn parse_age(text: &str) -> Result<u64, String> {
    let error = || "expected a number and s, m, h, d or w, such as 12h".to_owned();
    let split = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(error)?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().map_err(|_| error())?;
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(error()),
    };
    Ok(now().saturating_sub(number.saturating_mul(unit)))
}

/// `owner/name` as the API's path.
fn repository_path(repository: &str) -> Result<String, Error> {
    match repository.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok(format!("/v1/repositories/{owner}/{name}"))
        }
        _ => Err(Error::Local(format!("{repository}: expected owner/name"))),
    }
}

fn print_json(value: &impl Serialize) -> Result<ExitCode, Error> {
    let text =
        serde_json::to_string_pretty(value).map_err(|error| Error::Local(error.to_string()))?;
    output::line(text);
    Ok(ExitCode::SUCCESS)
}

fn exit(success: bool) -> ExitCode {
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn run(client: &Client, json: bool, command: Command) -> Result<ExitCode, Error> {
    let paint = output::paint();
    match command {
        Command::Status => {
            let status: Status = client.get("/v1/status").await?;
            if json {
                return print_json(&status);
            }
            output::text(&human::status(&status, now(), paint));
        }
        Command::Jobs {
            repo,
            failed,
            conclusion,
            state,
            since,
            before,
            limit,
        } => {
            let query = JobQuery {
                repository: repo,
                state,
                conclusion: if failed {
                    Some("failure".to_owned())
                } else {
                    conclusion
                },
                since,
                until: None,
                before,
                limit,
            };
            let page: JobPage = client.get_query("/v1/jobs", &query).await?;
            if json {
                return print_json(&page);
            }
            output::text(&human::jobs(&page, now(), paint));
        }
        Command::Job { id } => {
            let job: Job = client.get(&format!("/v1/jobs/{id}")).await?;
            if json {
                return print_json(&job);
            }
            output::text(&human::job(&job, now(), paint));
        }
        Command::Logs { id, follow, from } => logs(client, id, follow, from.unwrap_or(0)).await?,
        Command::Watch => watch(client, json, paint).await?,
        Command::Pause { repository, force } => {
            return pause(client, json, &repository, force).await;
        }
        Command::Resume { repository } => {
            client
                .post(&format!("{}/resume", repository_path(&repository)?), &())
                .await?;
            if !json {
                output::line(format_args!("{repository} resumed"));
            }
        }
        Command::Reseed { profile, no_wait } => {
            return reseed(client, json, &profile, no_wait).await;
        }
        Command::Caches {
            clean: Some(volume),
        } => {
            client
                .post(&format!("/v1/caches/{volume}/clean"), &())
                .await?;
            if !json {
                output::line(format_args!("{volume} emptied"));
            }
        }
        Command::Caches { clean: None } => {
            let caches: Caches = client.get("/v1/caches").await?;
            if json {
                return print_json(&caches);
            }
            output::text(&human::caches(&caches, now(), paint));
        }
        Command::Config { command } => return config(client, json, command).await,
        Command::Doctor => return doctor(client, json).await,
        Command::Web { no_open } => return web(client, no_open),
    }
    Ok(ExitCode::SUCCESS)
}

async fn pause(
    client: &Client,
    json: bool,
    repository: &str,
    force: bool,
) -> Result<ExitCode, Error> {
    let path = format!("{}/pause", repository_path(repository)?);
    let paused: Paused = match client.post_json(&path, &PauseQuery { force }).await {
        Ok(paused) => paused,
        Err(ClientError::Api { status, message }) if status == hyper::StatusCode::CONFLICT => {
            return Err(Error::Local(format!(
                "{message}\n`runnerctl pause --force {repository}` cancels its runs and removes their runners"
            )));
        }
        Err(error) => return Err(error.into()),
    };
    if json {
        return print_json(&paused);
    }
    output::line(format_args!("{repository} paused: it gets no new runners"));
    for stopped in &paused.stopped {
        output::line(format_args!("stopped {stopped}"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks for a reseed and, unless `no_wait`, waits for its `cache.reseeded`. The event stream is open before the
/// request, so the event cannot be missed.
async fn reseed(
    client: &Client,
    json: bool,
    profile: &str,
    no_wait: bool,
) -> Result<ExitCode, Error> {
    let mut events = if no_wait {
        None
    } else {
        Some(client.events(EventQuery::default(), None).await?)
    };
    client
        .post(&format!("/v1/profiles/{profile}/reseed"), &())
        .await?;
    let Some(events) = &mut events else {
        if !json {
            output::line(format_args!(
                "{profile}: reseed requested; it runs once the profile's slots are free"
            ));
        }
        return Ok(ExitCode::SUCCESS);
    };
    if !json {
        output::line(format_args!(
            "{profile}: reseeding once its slots are free; Ctrl-C stops waiting, not the reseed"
        ));
    }
    loop {
        let envelope = events.next().await?;
        let Event::CacheReseeded {
            profile: reseeded,
            output,
            error,
        } = &envelope.event
        else {
            continue;
        };
        if reseeded != profile {
            continue;
        }
        if json {
            print_json(&envelope)?;
        } else {
            let output = output.trim();
            if !output.is_empty() {
                output::line(output);
            }
            match error {
                Some(error) => output::line(format_args!("{profile}: reseed failed: {error}")),
                None => output::line(format_args!("{profile}: reseeded")),
            };
        }
        return Ok(exit(error.is_none()));
    }
}

async fn config(client: &Client, json: bool, command: ConfigCommand) -> Result<ExitCode, Error> {
    let (path, text) = match command {
        ConfigCommand::Show => {
            let file: ConfigFile = client.get("/v1/config").await?;
            if json {
                return print_json(&file);
            }
            output::line(format_args!("# {}", file.path));
            output::text(&file.text);
            return Ok(ExitCode::SUCCESS);
        }
        ConfigCommand::Check { file: Some(path) } => {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                Error::Local(format!("cannot read {}: {error}", path.display()))
            })?;
            (path.display().to_string(), text)
        }
        ConfigCommand::Check { file: None } => {
            let file: ConfigFile = client.get("/v1/config").await?;
            (file.path, file.text)
        }
    };
    let checked: ConfigCheck = client.post_toml("/v1/config/check", text).await?;
    if json {
        print_json(&checked)?;
    } else {
        for warning in &checked.warnings {
            output::line(format_args!("warning: {warning}"));
        }
        output::line(format_args!("{path}: {}", checked.message));
    }
    Ok(exit(checked.valid))
}

/// The daemon's checks; when it is not running, `runnerd doctor` run here, which checks the same.
async fn doctor(client: &Client, json: bool) -> Result<ExitCode, Error> {
    let doctor: Doctor = match client.get("/v1/doctor").await {
        Ok(doctor) => doctor,
        Err(ClientError::NotRunning(_)) if !json => {
            output::line("runnerd is not running; running `runnerd doctor` here");
            let status = std::process::Command::new("runnerd")
                .arg("doctor")
                .status()
                .map_err(|error| {
                    Error::Local(if error.kind() == std::io::ErrorKind::NotFound {
                        "runnerd is not on PATH, so its checks cannot run here either".to_owned()
                    } else {
                        format!("cannot run runnerd doctor: {error}")
                    })
                })?;
            return Ok(exit(status.success()));
        }
        Err(error) => return Err(error.into()),
    };
    let passed = !doctor
        .checks
        .iter()
        .any(|check| check.status == CheckStatus::Fail);
    if json {
        print_json(&doctor)?;
    } else {
        for check in &doctor.checks {
            output::line(check);
        }
    }
    Ok(exit(passed))
}

/// Opens the dashboard with the one-time login URL the daemon writes next to its socket.
fn web(client: &Client, no_open: bool) -> Result<ExitCode, Error> {
    let file = client.socket().with_extension("web");
    let url = match std::fs::read_to_string(&file) {
        Ok(text) => text.trim().to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Local(format!(
                "no {}: runnerd is not running, or was started with --web off",
                file.display()
            )));
        }
        Err(error) => {
            return Err(Error::Local(format!(
                "cannot read {}: {error}",
                file.display()
            )));
        }
    };
    if no_open {
        output::line(url);
        return Ok(ExitCode::SUCCESS);
    }
    match std::process::Command::new("xdg-open").arg(&url).status() {
        Ok(status) if status.success() => {
            output::line("opened the dashboard in your browser");
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(Error::Local(format!(
            "cannot open a browser with xdg-open; open this URL, which works once: {url}"
        ))),
    }
}

/// Prints the log from line `from`. Following, it reads until the daemon ends the response, which it does once the
/// job's record is final; if the response ended for another reason (a daemon restart), it asks again from the next
/// line. A response cut short after the job became final is read once more, for the lines it missed.
async fn logs(client: &Client, id: u64, follow: bool, mut from: u64) -> Result<(), Error> {
    let mut last_read = false;
    loop {
        let mut lines = client
            .logs(
                id,
                &LogQuery {
                    from: Some(from),
                    follow,
                },
            )
            .await?;
        let ended = loop {
            match lines.next().await {
                Ok(Some(line)) => {
                    if !output::line(line) {
                        return Ok(());
                    }
                    from += 1;
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        if !follow || last_read {
            return ended.map_err(Error::from);
        }
        let job: Job = client.get(&format!("/v1/jobs/{id}")).await?;
        if job.summary.state.is_final() {
            if ended.is_ok() {
                return Ok(());
            }
            last_read = true;
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn watch(client: &Client, json: bool, paint: human::Paint) -> Result<(), Error> {
    let mut events = client.events(EventQuery::default(), None).await?;
    if json || !output::is_terminal() {
        loop {
            let envelope = events.next().await?;
            let line = if json {
                serde_json::to_string(&envelope).ok()
            } else {
                human::event_line(&envelope, paint)
            };
            if let Some(line) = line
                && !output::line(line)
            {
                return Ok(());
            }
        }
    }
    let mut state = watch::State::read(client).await?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        if !output::redraw(&watch::frame(&state, now(), paint)) {
            return Ok(());
        }
        tokio::select! {
            envelope = events.next() => {
                if let Some(id) = state.apply(&envelope?) {
                    state.refresh(client.get(&format!("/v1/jobs/{id}")).await?);
                }
            }
            _ = tick.tick() => {}
        }
    }
}
