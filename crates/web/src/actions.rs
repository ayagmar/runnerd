//! The dashboard's writes: pause (forced or not) and resume of one repository or every one, reseed, empty a cache,
//! check and save the config. Each passes the request on to the daemon's API and answers with HTML Datastar patches
//! in: the region as it is now, or a notice saying what the daemon refused and why. Datastar applies only 200
//! responses, so a refusal is a 200 with the notice. Nothing here decides what is allowed; the daemon does.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use maud::Markup;
use runnerd_core::api::{Busy, ConfigCheck, ConfigFile, PauseQuery, Paused, RunningJob, Status};
use serde::Deserialize;

use crate::App;
use crate::daemon::{Body, Reply};
use crate::views::config::{self, Outcome};
use crate::views::{self, Tone, overview};

fn html(markup: &[Markup]) -> Response {
    Html(
        markup
            .iter()
            .map(|markup| markup.clone().into_string())
            .collect::<String>(),
    )
    .into_response()
}

fn refused(message: &str) -> Response {
    html(&[views::notice(message, Tone::Failed)])
}

/// Sends a request without a body; a failure to reach the daemon is a refusal too.
async fn send(app: &App, path: &str) -> Result<Reply, Markup> {
    app.daemon
        .request(Method::POST, path, None)
        .await
        .map_err(|error| views::notice(&error.to_string(), Tone::Failed))
}

/// The repository's row as the daemon reports it now.
async fn row(app: &App, repository: &str) -> Result<Markup, Markup> {
    let status: Status = app
        .daemon
        .get("/v1/status")
        .await
        .map_err(|error| views::notice(&error.to_string(), Tone::Failed))?;
    status
        .repositories
        .iter()
        .find(|candidate| candidate.name == repository)
        .map(|found| overview::repository_row(&status, found))
        .ok_or_else(|| {
            views::notice(
                &format!("{repository} is not a configured repository"),
                Tone::Failed,
            )
        })
}

/// Every repository's row as the daemon reports it now.
async fn rows(app: &App) -> Result<Markup, Markup> {
    let status: Status = app
        .daemon
        .get("/v1/status")
        .await
        .map_err(|error| views::notice(&error.to_string(), Tone::Failed))?;
    Ok(overview::repositories(&status))
}

/// The question a pause refused with 409 asks: the jobs it waits for, by repository, and whether to cancel them
/// with `force`. The daemon's message tells a CLI user how to force it; the question asks instead.
fn ask_to_force(reply: &Reply, force: &str, label: &str, dismiss: &str) -> Response {
    let message = match serde_json::from_slice::<Busy>(&reply.body) {
        Ok(busy) if !busy.jobs.is_empty() => {
            let mut repositories: Vec<(&str, Vec<String>)> = Vec::new();
            for job in &busy.jobs {
                match repositories
                    .iter_mut()
                    .find(|(repository, _)| *repository == job.repository)
                {
                    Some((_, jobs)) => jobs.push(job.to_string()),
                    None => repositories.push((&job.repository, vec![job.to_string()])),
                }
            }
            let runs: Vec<String> = repositories
                .iter()
                .map(|(repository, jobs)| format!("{repository} runs {}", jobs.join(", ")))
                .collect();
            let them = if busy.jobs.len() == 1 { "it" } else { "them" };
            format!("{}. Pausing now cancels {them} on GitHub.", runs.join("; "))
        }
        _ => reply.message(),
    };
    html(&[views::question(
        &message,
        &format!("@post('{force}')"),
        label,
        dismiss,
    )])
}

/// The page after a pause: `region` and, if it cancelled runs, which.
fn paused(reply: &Reply, region: Markup, what: &str) -> Response {
    let stopped: Vec<RunningJob> = serde_json::from_slice::<Paused>(&reply.body)
        .map(|paused| paused.stopped)
        .unwrap_or_default();
    if stopped.is_empty() {
        html(&[region])
    } else {
        let names: Vec<String> = stopped.iter().map(ToString::to_string).collect();
        html(&[
            region,
            views::notice(
                &format!("Paused {what}; stopped {}", names.join(", ")),
                Tone::Done,
            ),
        ])
    }
}

/// `POST /v1/repositories/{owner}/{name}/pause?force=`. A pause the daemon refuses because a job runs asks
/// whether to cancel the job.
pub(crate) async fn pause(
    State(app): State<App>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<PauseQuery>,
) -> Response {
    let repository = format!("{owner}/{name}");
    let path = format!(
        "/v1/repositories/{repository}/pause{}",
        if query.force { "?force=true" } else { "" }
    );
    let reply = match send(&app, &path).await {
        Ok(reply) => reply,
        Err(notice) => return html(&[notice]),
    };
    if reply.status == StatusCode::CONFLICT && !query.force {
        return ask_to_force(
            &reply,
            &format!("/v1/repositories/{repository}/pause?force=true"),
            "Cancel and pause",
            "Keep it running",
        );
    }
    if !reply.status.is_success() {
        return refused(&reply.message());
    }
    match row(&app, &repository).await {
        Ok(row) => paused(&reply, row, &repository),
        Err(notice) => html(&[notice]),
    }
}

/// `POST /v1/repositories/pause?force=`: pauses every repository, or, while any runs a job, none and asks whether to
/// cancel the jobs.
pub(crate) async fn pause_all(State(app): State<App>, Query(query): Query<PauseQuery>) -> Response {
    let path = if query.force {
        "/v1/repositories/pause?force=true"
    } else {
        "/v1/repositories/pause"
    };
    let reply = match send(&app, path).await {
        Ok(reply) => reply,
        Err(notice) => return html(&[notice]),
    };
    if reply.status == StatusCode::CONFLICT && !query.force {
        return ask_to_force(
            &reply,
            "/v1/repositories/pause?force=true",
            "Cancel and pause all",
            "Keep them running",
        );
    }
    if !reply.status.is_success() {
        return refused(&reply.message());
    }
    match rows(&app).await {
        Ok(rows) => paused(&reply, rows, "every repository"),
        Err(notice) => html(&[notice]),
    }
}

/// `POST /v1/repositories/resume`.
pub(crate) async fn resume_all(State(app): State<App>) -> Response {
    match send(&app, "/v1/repositories/resume").await {
        Ok(reply) if reply.status.is_success() => match rows(&app).await {
            Ok(rows) => html(&[rows]),
            Err(notice) => html(&[notice]),
        },
        Ok(reply) => refused(&reply.message()),
        Err(notice) => html(&[notice]),
    }
}

/// `POST /v1/repositories/{owner}/{name}/resume`.
pub(crate) async fn resume(
    State(app): State<App>,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let repository = format!("{owner}/{name}");
    match send(&app, &format!("/v1/repositories/{repository}/resume")).await {
        Ok(reply) if reply.status.is_success() => match row(&app, &repository).await {
            Ok(row) => html(&[row]),
            Err(notice) => html(&[notice]),
        },
        Ok(reply) => refused(&reply.message()),
        Err(notice) => html(&[notice]),
    }
}

/// `POST /v1/profiles/{name}/reseed`.
pub(crate) async fn reseed(State(app): State<App>, Path(profile): Path<String>) -> Response {
    match send(&app, &format!("/v1/profiles/{profile}/reseed")).await {
        Ok(reply) if reply.status.is_success() => html(&[views::notice(
            &format!(
                "Reseeding {profile}'s snapshot from slot 1; the overview shows when it is done"
            ),
            Tone::Done,
        )]),
        Ok(reply) => refused(&reply.message()),
        Err(notice) => html(&[notice]),
    }
}

/// `POST /v1/caches/{name}/clean`.
pub(crate) async fn clean(State(app): State<App>, Path(volume): Path<String>) -> Response {
    match send(&app, &format!("/v1/caches/{volume}/clean")).await {
        Ok(reply) if reply.status.is_success() => html(&[views::notice(
            &format!("Emptied {volume}; its size shows at the next measurement"),
            Tone::Done,
        )]),
        Ok(reply) => refused(&reply.message()),
        Err(notice) => html(&[notice]),
    }
}

/// The editor's signals.
#[derive(Debug, Deserialize)]
pub(crate) struct Edit {
    toml: String,
}

/// `POST /config/check`: the result region for the text in the editor.
pub(crate) async fn check_config(State(app): State<App>, Json(edit): Json<Edit>) -> Response {
    match app.daemon.check(edit.toml.clone()).await {
        Ok(check) => html(&[config::result(&check, &edit.toml, &Outcome::Checked)]),
        Err(error) => refused(&error.to_string()),
    }
}

/// `PUT /config`: saves the text in the editor and has the daemon reload it, or says why the daemon would not.
pub(crate) async fn save_config(State(app): State<App>, Json(edit): Json<Edit>) -> Response {
    let reply = match app
        .daemon
        .request(
            Method::PUT,
            "/v1/config",
            Some(Body::toml(edit.toml.clone())),
        )
        .await
    {
        Ok(reply) => reply,
        Err(error) => return refused(&error.to_string()),
    };
    let check = serde_json::from_slice::<ConfigCheck>(&reply.body);
    match (reply.status, check) {
        (status, Ok(check)) if status.is_success() => {
            let path = app
                .daemon
                .get::<ConfigFile>("/v1/config")
                .await
                .map_or_else(|_| "runnerd.toml".to_owned(), |file| file.path);
            html(&[config::result(&check, &edit.toml, &Outcome::Saved(path))])
        }
        (StatusCode::UNPROCESSABLE_ENTITY, Ok(check)) => {
            html(&[config::result(&check, &edit.toml, &Outcome::Refused)])
        }
        _ => refused(&format!("Not saved: {}", reply.message())),
    }
}
