//! `runnerctl`: the command-line client. It talks to the daemon's API only, never to Docker or GitHub. Commands map
//! to API calls here; the transport is `runnerd_core::client`, text for people `human`, the live view `watch`, the
//! full-screen dashboard `tui`, printing `output`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use runnerd_core::api::{
    Caches, CheckStatus, ConfigCheck, ConfigFile, Doctor, Event, EventQuery, Job, JobPage,
    JobQuery, JobState, LogQuery, PauseQuery, Paused, Records, RemoveRecordsQuery, RemovedRecords,
    Resize, ShellQuery, Sizing, SizingQuery, Status,
};
use runnerd_core::config::{self, ByteSize, resize_profile};
use serde::Serialize;

mod human;
mod output;
#[cfg(test)]
mod testing;
#[cfg(feature = "tui")]
mod tui;
mod watch;

use output::fail;
use runnerd_core::client::{Body, Client, ClientError, Method, StatusCode};

/// Command-line client for the runnerd daemon.
#[derive(Parser)]
#[command(name = "runnerctl", version)]
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
    /// Full-screen dashboard: budget, slots with their memory and CPU, history, and each job's steps and log.
    ///
    /// Keys: 1 overview, 2 history, tab and j/k to move, enter to open a job, p/r to pause or resume, ? for all.
    #[cfg(feature = "tui")]
    Tui,
    /// Give a repository, or every one, no new runners, also after a restart. Refused while it runs a job, unless
    /// --force.
    Pause {
        /// owner/name
        #[arg(required_unless_present = "all")]
        repository: Option<String>,
        /// Pause every configured repository; refused while any of them runs a job, unless --force.
        #[arg(long, conflicts_with = "repository")]
        all: bool,
        /// Cancel the running jobs' workflow runs and remove their runners.
        #[arg(long)]
        force: bool,
    },
    /// Let a paused repository, or every one, get runners again.
    Resume {
        /// owner/name
        #[arg(required_unless_present = "all")]
        repository: Option<String>,
        /// Resume every configured repository.
        #[arg(long, conflicts_with = "repository")]
        all: bool,
    },
    /// Make a profile's slot 1 layers its new snapshot, once its slots are free; waits until it is done.
    Reseed {
        profile: String,
        /// Return once the daemon accepted the request.
        #[arg(long)]
        no_wait: bool,
    },
    /// Open a shell in a failed job's record: its runner as the job left it, in the job's workspace.
    ///
    /// Shows what the failed step ran first. Exiting the shell removes its container.
    Shell {
        job: u64,
        /// Show what this step ran instead, by its number in `runnerctl job`.
        #[arg(long)]
        step: Option<u32>,
    },
    /// List the records of failed jobs, their sizes and the limits they are kept within.
    Records {
        #[command(subcommand)]
        command: Option<RecordsCommand>,
    },
    /// Show what each profile's jobs use: percentiles of their sampled memory and CPU peaks, how long they waited
    /// and how busy the slots were, and what to set each profile's memory and slots to.
    Sizing {
        /// Only jobs queued in the last 30m, 12h, 7d, 2w, … [default: 30d].
        #[arg(long, value_parser = parse_age)]
        since: Option<u64>,
        #[command(subcommand)]
        command: Option<SizingCommand>,
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
enum RecordsCommand {
    /// Remove records and free the disk they take.
    Rm {
        /// The job whose record goes.
        #[arg(required_unless_present_any = ["all", "older_than"])]
        job: Option<u64>,
        /// Remove every record, but those a shell is open in.
        #[arg(long, conflicts_with_all = ["job", "older_than"])]
        all: bool,
        /// Remove the records older than this, such as 2d or 12h.
        #[arg(long, value_name = "AGE", conflicts_with = "job")]
        older_than: Option<String>,
    },
}

#[derive(Subcommand)]
#[allow(
    clippy::doc_markdown,
    reason = "clap prints these comments as --help, where backticks would show"
)]
enum SizingCommand {
    /// Write a profile's advised memory and slots into the daemon's config, after showing the change and asking.
    ///
    /// The daemon checks the new config against the slice's MemoryMax before it writes and reloads it, as for any
    /// other config.
    Apply {
        profile: String,
        /// Write it without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Write back a profile's memory and slots from before auto sizing last changed them, after showing the change
    /// and asking.
    Undo {
        profile: String,
        /// Write it without asking.
        #[arg(long)]
        yes: bool,
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
    config::parse_age(text).map(|age| now().saturating_sub(age.as_secs()))
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

/// What a pause or resume names: one repository, or every one with `--all`.
pub(crate) struct Target {
    /// The route's path before `/pause` or `/resume`.
    pub(crate) path: String,
    /// For people to read.
    pub(crate) name: String,
    /// The arguments that name it, for a hint to run again.
    arguments: String,
}

impl Target {
    /// `repository`, or every one when `None`; clap requires it or `--all`.
    pub(crate) fn new(repository: Option<String>) -> Result<Target, Error> {
        Ok(match repository {
            Some(repository) => Target {
                path: repository_path(&repository)?,
                arguments: repository.clone(),
                name: repository,
            },
            None => Target {
                path: "/v1/repositories".to_owned(),
                name: "every repository".to_owned(),
                arguments: "--all".to_owned(),
            },
        })
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
        Command::Logs { id, follow, from } => {
            logs(client, id, follow, from.unwrap_or(0), output::line).await?;
        }
        Command::Watch => watch(client, json, paint).await?,
        #[cfg(feature = "tui")]
        Command::Tui => {
            if json || !output::is_terminal() {
                return Err(Error::Local(
                    "runnerctl tui needs a terminal; `runnerctl watch` prints events as lines"
                        .to_owned(),
                ));
            }
            tui::run(client, paint.colour).await?;
        }
        Command::Pause {
            repository, force, ..
        } => {
            return pause(client, json, &Target::new(repository)?, force).await;
        }
        Command::Resume { repository, .. } => {
            let target = Target::new(repository)?;
            client.post(&format!("{}/resume", target.path), &()).await?;
            if !json {
                output::line(format_args!("{} resumed", target.name));
            }
        }
        Command::Reseed { profile, no_wait } => {
            return reseed(client, json, &profile, no_wait).await;
        }
        Command::Shell { job, step } => return shell(client, job, step).await,
        Command::Records { command } => return records(client, json, command).await,
        Command::Sizing { since, command } => return sizing(client, json, since, command).await,
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

/// `runnerctl records`: lists the records of failed jobs, or removes some.
async fn records(
    client: &Client,
    json: bool,
    command: Option<RecordsCommand>,
) -> Result<ExitCode, Error> {
    let Some(RecordsCommand::Rm {
        job,
        all,
        older_than,
    }) = command
    else {
        let records: Records = client.get("/v1/records").await?;
        if json {
            return print_json(&records);
        }
        output::text(&human::records(&records, now(), output::paint()));
        return Ok(ExitCode::SUCCESS);
    };
    let path = match job {
        Some(job) => format!("/v1/records/{job}"),
        None => with_query("/v1/records", &RemoveRecordsQuery { all, older_than })?,
    };
    let removed = remove_records(client, &path).await?;
    if json {
        return print_json(&removed);
    }
    output::line(human::removed(&removed));
    Ok(ExitCode::SUCCESS)
}

/// Removes the records `path` names, `/v1/records/{job}` or `/v1/records?…`.
async fn remove_records(client: &Client, path: &str) -> Result<RemovedRecords, Error> {
    let reply = client.request(Method::DELETE, path, None).await?;
    if !reply.status.is_success() {
        return Err(Error::Local(reply.message()));
    }
    serde_json::from_slice(&reply.body).map_err(|error| Error::Local(error.to_string()))
}

/// `path?query`, as the API reads it.
fn with_query(path: &str, query: &impl Serialize) -> Result<String, Error> {
    let query =
        serde_urlencoded::to_string(query).map_err(|error| Error::Local(error.to_string()))?;
    Ok(if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    })
}

/// Opens a shell in a job's record and carries this terminal's keys to it and its output here, in raw mode, until
/// the shell exits or the daemon closes it.
async fn shell(client: &Client, job: u64, step: Option<u32>) -> Result<ExitCode, Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let size = output::terminal_size();
    let query = ShellQuery {
        step,
        cols: size.map(|(cols, _)| cols),
        rows: size.map(|(_, rows)| rows),
    };
    let terminal = client.shell(job, &query).await?;
    let (mut from_shell, mut to_shell) = tokio::io::split(terminal);
    let _raw = output::raw_mode();
    let mut typed = output::stdin_chunks();
    let shown = async {
        let mut buffer = vec![0; 8192];
        loop {
            match from_shell.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if !output::bytes(&buffer[..read]) {
                        break;
                    }
                }
            }
        }
    };
    let carried = async {
        while let Some(chunk) = typed.recv().await {
            if to_shell.write_all(&chunk).await.is_err() {
                break;
            }
        }
        // The keyboard closed: the shell's input ends, and with it the shell.
        let _ = to_shell.shutdown().await;
        std::future::pending::<()>().await;
    };
    tokio::select! {
        () = shown => {}
        () = carried => {}
    }
    Ok(ExitCode::SUCCESS)
}

async fn pause(
    client: &Client,
    json: bool,
    target: &Target,
    force: bool,
) -> Result<ExitCode, Error> {
    let path = format!("{}/pause", target.path);
    let paused: Paused = match client.post_json(&path, &PauseQuery { force }).await {
        Ok(paused) => paused,
        Err(ClientError::Api {
            status,
            message,
            jobs,
        }) if status == StatusCode::CONFLICT => {
            let (runs, runners) = if jobs.len() == 1 {
                ("its run", "its runner")
            } else {
                ("their runs", "their runners")
            };
            return Err(Error::Local(format!(
                "{message}\n`runnerctl pause --force {}` cancels {runs} and removes {runners}",
                target.arguments
            )));
        }
        Err(error) => return Err(error.into()),
    };
    if json {
        return print_json(&paused);
    }
    output::line(format_args!(
        "{} paused: it gets no new runners",
        target.name
    ));
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

/// `runnerctl sizing`: shows the advice, or writes one profile's into the config.
async fn sizing(
    client: &Client,
    json: bool,
    since: Option<u64>,
    command: Option<SizingCommand>,
) -> Result<ExitCode, Error> {
    let query = SizingQuery {
        since: since.map(|age| now().saturating_sub(age)),
    };
    let sizing: Sizing = client.get_query("/v1/sizing", &query).await?;
    let (profile, yes, undo) = match command {
        None => {
            if json {
                return print_json(&sizing);
            }
            output::text(&human::sizing(&sizing, now(), output::paint()));
            return Ok(ExitCode::SUCCESS);
        }
        Some(SizingCommand::Apply { profile, yes }) => (profile, yes, false),
        Some(SizingCommand::Undo { profile, yes }) => (profile, yes, true),
    };
    let Some(advised) = sizing.profiles.iter().find(|p| p.name == profile) else {
        let names: Vec<_> = sizing.profiles.iter().map(|p| p.name.as_str()).collect();
        return Err(Error::Local(format!(
            "no profile {profile:?}; the config has {}",
            names.join(", ")
        )));
    };
    let (line, resize) = if undo {
        let Some(change) = sizing
            .changes
            .iter()
            .rfind(|change| change.profile == profile)
        else {
            return Err(Error::Local(format!(
                "auto sizing changed {profile} in none of the history read"
            )));
        };
        (
            format!(
                "{profile}: undo auto sizing's {}, {}",
                change.line(),
                human::ago(change.at, now())
            ),
            Some(change.from),
        )
    } else {
        (
            format!("{profile}: {}", sizing.advice_line(advised)),
            advised.advice.change(),
        )
    };
    let Some(resize) = resize else {
        if !json {
            output::line(format_args!("{line}; nothing to change"));
        }
        return Ok(ExitCode::SUCCESS);
    };
    if !yes {
        if json || !std::io::stdin().is_terminal() {
            return Err(Error::Local(
                "no terminal to ask on; pass --yes to write the change".to_owned(),
            ));
        }
        output::line(&line);
        output::text(&format!(
            "[profiles.{profile}]\n  memory = \"{}\" -> \"{}\"\n  slots = {} -> {}\nWrite it into the daemon's config? [y/N] ",
            ByteSize(advised.memory),
            ByteSize(resize.memory),
            advised.slots,
            resize.slots
        ));
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|error| Error::Local(error.to_string()))?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            output::line("not written");
            return Ok(ExitCode::FAILURE);
        }
    }
    let checked = write_resize(client, &profile, resize).await?;
    if json {
        return print_json(&checked);
    }
    for warning in &checked.warnings {
        output::line(format_args!("warning: {warning}"));
    }
    output::line(format_args!(
        "{profile}: memory {}, {} slots written; the daemon runs with them",
        ByteSize(resize.memory),
        resize.slots
    ));
    if undo && sizing.auto {
        output::line(
            "auto sizing is on: it may change them again after [sizing] interval; bound the profile with \
             min_memory, max_memory or max_slots to keep them",
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes a profile's new memory and slots into the daemon's config through `PUT /v1/config`, which checks it
/// against the slice's budget first; the rest of the file stays as written. A config replaced between the read and
/// the write, as by auto sizing, is not overwritten.
pub(crate) async fn write_resize(
    client: &Client,
    profile: &str,
    resize: Resize,
) -> Result<ConfigCheck, Error> {
    let file: ConfigFile = client.get("/v1/config").await?;
    let text = resize_profile(&file.text, profile, resize)
        .map_err(|error| Error::Local(error.to_string()))?;
    let path = match &file.revision {
        Some(revision) => format!("/v1/config?revision={revision}"),
        None => "/v1/config".to_owned(),
    };
    let reply = client
        .request(Method::PUT, &path, Some(Body::toml(text)))
        .await?;
    match serde_json::from_slice::<ConfigCheck>(&reply.body) {
        Ok(checked) if reply.status.is_success() => Ok(checked),
        Ok(checked) => Err(Error::Local(format!(
            "the daemon refused the new config: {}",
            checked.message
        ))),
        Err(_) => Err(Error::Local(reply.message())),
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

/// Hands the log from line `from` to `line`, until it returns false. Following, it reads until the daemon ends the
/// response, which it does once the job's record is final; if the response ended for another reason (a daemon
/// restart), it asks again from the next line. While the daemon is away it keeps asking every second, so a restart
/// of any length loses no line and repeats none; only the first request fails when the daemon is not there. A
/// response cut short after the job became final is read once more, for the lines it missed.
async fn logs(
    client: &Client,
    id: u64,
    follow: bool,
    mut from: u64,
    mut line: impl FnMut(String) -> bool,
) -> Result<(), Error> {
    const AGAIN: Duration = Duration::from_secs(1);
    let mut last_read = false;
    let mut answered = false;
    loop {
        let query = LogQuery {
            from: Some(from),
            follow,
        };
        let mut lines = match client.logs(id, &query).await {
            Ok(lines) => lines,
            Err(error) if follow && answered && is_outage(&error) => {
                tokio::time::sleep(AGAIN).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        answered = true;
        let ended = loop {
            match lines.next().await {
                Ok(Some(text)) => {
                    if !line(text) {
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
        let job: Job = match client.get(&format!("/v1/jobs/{id}")).await {
            Ok(job) => job,
            Err(error) if is_outage(&error) => {
                tokio::time::sleep(AGAIN).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if job.summary.state.is_final() {
            if ended.is_ok() {
                return Ok(());
            }
            last_read = true;
        } else {
            tokio::time::sleep(AGAIN).await;
        }
    }
}

/// Whether the daemon is away, as while it restarts, rather than refusing.
fn is_outage(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::NotRunning(_) | ClientError::Transport(_) | ClientError::Timeout(_)
    )
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
    watch::run(client, events, |frame| {
        output::redraw(&watch::frame(frame, now(), paint))
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests fail by panicking")]

    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::{RawQuery, State};
    use axum::routing::get;
    use futures_util::{StreamExt, stream};
    use runnerd_core::api::{Job, JobMetrics, JobState, JobSummary, LogSummary, Timeline};
    use tokio::sync::Notify;

    use crate::logs;
    use crate::testing::Fake;

    fn job(state: JobState) -> Job {
        Job {
            admission: None,
            summary: JobSummary {
                id: 9001,
                repository: "halyard/ledger".to_owned(),
                run_id: 77,
                run_attempt: 1,
                workflow: None,
                name: "test".to_owned(),
                branch: None,
                url: None,
                labels: Vec::new(),
                state,
                conclusion: state.is_final().then(|| "success".to_owned()),
                runner: None,
                timeline: Timeline::default(),
                metrics: JobMetrics::default(),
                log: LogSummary::default(),
            },
            steps: Vec::new(),
            record: None,
        }
    }

    /// What the daemon was asked for, across its restart.
    #[derive(Default)]
    struct Asked {
        log_queries: Mutex<Vec<String>>,
        /// Told when the first log response is under way; it ends once `stopped` is told.
        serving: Notify,
        stopped: Notify,
    }

    #[tokio::test]
    async fn a_followed_log_carries_on_after_the_daemon_was_down_for_a_while() {
        let asked = Arc::new(Asked::default());
        // Before the outage: two lines, then the response ends with the daemon, still running its job.
        let before = Router::new()
            .route(
                "/v1/jobs/9001/logs",
                get(
                    |State(asked): State<Arc<Asked>>, RawQuery(query): RawQuery| async move {
                        asked
                            .log_queries
                            .lock()
                            .unwrap()
                            .push(query.unwrap_or_default());
                        asked.serving.notify_one();
                        let end = async move { asked.stopped.notified().await };
                        Body::from_stream(
                            stream::iter([Ok::<_, Infallible>(Bytes::from("a\nb\n"))])
                                .chain(stream::once(end).filter_map(|()| async { None })),
                        )
                    },
                ),
            )
            .with_state(asked.clone());
        // After it: the job has finished, and its last line is there.
        let after = Router::new()
            .route(
                "/v1/jobs/9001/logs",
                get(
                    |State(asked): State<Arc<Asked>>, RawQuery(query): RawQuery| async move {
                        asked
                            .log_queries
                            .lock()
                            .unwrap()
                            .push(query.unwrap_or_default());
                        "c\n"
                    },
                ),
            )
            .route(
                "/v1/jobs/9001",
                get(|| async { axum::Json(job(JobState::Completed)) }),
            )
            .with_state(asked.clone());

        let mut fake = Fake::serve(before);
        let client = fake.client(Duration::from_secs(5), Duration::from_secs(5));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let printed = lines.clone();
        let reader = tokio::spawn(async move {
            logs(&client, 9001, true, 0, |line| {
                printed.lock().unwrap().push(line);
                true
            })
            .await
        });
        asked.serving.notified().await;
        fake.stop().await;
        // The response ends now, so the reader asks about the job while nothing listens.
        asked.stopped.notify_one();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        fake.serve_again(after);

        let read = tokio::time::timeout(Duration::from_secs(10), reader)
            .await
            .expect("the reader should finish once the daemon is back")
            .unwrap();
        assert!(
            read.is_ok(),
            "{:?}",
            read.err().map(|error| error.to_string())
        );
        assert_eq!(*lines.lock().unwrap(), ["a", "b", "c"]);
        let queries = asked.log_queries.lock().unwrap();
        assert_eq!(
            queries.first().map(String::as_str),
            Some("from=0&follow=true")
        );
        assert_eq!(
            queries.last().map(String::as_str),
            Some("from=2&follow=true")
        );
    }
}
