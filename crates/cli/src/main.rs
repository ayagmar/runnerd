//! `runnerctl`: the command-line client. It talks to the daemon's API only, never to Docker or GitHub. Commands map
//! to API calls here; the transport is `client`, text for people `human`, the live view `watch`, printing `output`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use runnerd_core::api::{Caches, EventQuery, Job, JobPage, JobQuery, JobState, LogQuery, Status};
use serde::Serialize;

mod client;
mod human;
mod output;
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
    /// Give a repository no new runners; its running jobs finish.
    Pause {
        /// owner/name
        repository: String,
    },
    /// Let a paused repository get runners again.
    Resume {
        /// owner/name
        repository: String,
    },
    /// Show the cache volumes, their sizes and limits.
    Caches,
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
        Ok(()) => ExitCode::SUCCESS,
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
fn repository_path(repository: &str) -> Result<String, ClientError> {
    match repository.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok(format!("/v1/repositories/{owner}/{name}"))
        }
        _ => Err(ClientError::Api {
            status: hyper::StatusCode::BAD_REQUEST,
            message: format!("{repository}: expected owner/name"),
        }),
    }
}

fn print_json(value: &impl Serialize) -> Result<(), ClientError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| ClientError::Decode(error.to_string()))?;
    output::line(text);
    Ok(())
}

async fn run(client: &Client, json: bool, command: Command) -> Result<(), ClientError> {
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
        Command::Pause { repository } => {
            client
                .post(
                    &format!("{}/pause", repository_path(&repository)?),
                    None::<&()>,
                )
                .await?;
            if !json {
                output::line(format_args!(
                    "{repository} paused: it gets no new runners; its running jobs finish"
                ));
            }
        }
        Command::Resume { repository } => {
            client
                .post(
                    &format!("{}/resume", repository_path(&repository)?),
                    None::<&()>,
                )
                .await?;
            if !json {
                output::line(format_args!("{repository} resumed"));
            }
        }
        Command::Caches => {
            let caches: Caches = client.get("/v1/caches").await?;
            if json {
                return print_json(&caches);
            }
            output::text(&human::caches(&caches, now(), paint));
        }
    }
    Ok(())
}

/// Prints the log from line `from`. Following, it reads until the daemon ends the response, which it does once the
/// job's record is final; if the response ended for another reason (a daemon restart), it asks again from the next
/// line.
async fn logs(client: &Client, id: u64, follow: bool, mut from: u64) -> Result<(), ClientError> {
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
        if !follow {
            return ended;
        }
        let job: Job = client.get(&format!("/v1/jobs/{id}")).await?;
        if job.summary.state.is_final() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn watch(client: &Client, json: bool, paint: human::Paint) -> Result<(), ClientError> {
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
            envelope = events.next() => state.apply(&envelope?),
            _ = tick.tick() => {}
        }
    }
}
