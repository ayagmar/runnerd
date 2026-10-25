//! The dashboard's writes: pause (forced or not) and resume of one repository or every one, reseed, empty a cache,
//! remove records, check and save the config. Each passes the request on to the daemon's API and answers with HTML Datastar patches
//! in: the region as it is now, or a notice saying what the daemon refused and why. Datastar applies only 200
//! responses, so a refusal is a 200 with the notice. Nothing here decides what is allowed; the daemon does.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use maud::Markup;
use runnerd_core::api::{
    Busy, ConfigCheck, ConfigFile, PauseQuery, Paused, Records, RemoveRecordsQuery, RemovedRecords,
    Status, force_pause_consequence, reseed_requested,
};
use runnerd_core::client::{Body, Reply};
use serde::Deserialize;

use crate::App;
use crate::views::config::{self, Outcome};
use crate::views::{self, Tone, caches, overview};

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

/// The question a pause refused with 409 asks: the jobs it waits for, as the daemon names them, and whether to
/// cancel their runs with `force`.
fn ask_to_force(reply: &Reply, force: &str, label: &str) -> Response {
    let (message, them) = match serde_json::from_slice::<Busy>(&reply.body) {
        Ok(busy) if !busy.jobs.is_empty() => (
            format!(
                "{}. Pausing now {}.",
                busy.message,
                force_pause_consequence(busy.jobs.len())
            ),
            if busy.jobs.len() == 1 { "it" } else { "them" },
        ),
        _ => (reply.message(), "them"),
    };
    html(&[views::question(
        &message,
        &format!("@post('{force}')"),
        label,
        &format!("Keep {them} running"),
    )])
}

/// The page after a pause: `region` and, if it cancelled runs, which.
fn paused(reply: &Reply, region: Markup, what: &str) -> Response {
    match serde_json::from_slice::<Paused>(&reply.body) {
        Ok(paused) if !paused.stopped.is_empty() => html(&[
            region,
            views::notice(&crate::format::sentence(&paused.line(what)), Tone::Done),
        ]),
        _ => html(&[region]),
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
                "{}; the overview shows when it is done",
                reseed_requested(&profile)
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

/// `DELETE /v1/records/{job}`.
pub(crate) async fn remove_record(State(app): State<App>, Path(job): Path<u64>) -> Response {
    remove_records(&app, &format!("/v1/records/{job}")).await
}

/// `DELETE /v1/records?all=true`; the dashboard removes no other selection.
pub(crate) async fn remove_all_records(
    State(app): State<App>,
    Query(query): Query<RemoveRecordsQuery>,
) -> Response {
    if !query.all {
        return refused("say which records: all=true");
    }
    remove_records(&app, "/v1/records?all=true").await
}

/// Removes the records `path` names and answers with the records section as it is now and what was freed.
async fn remove_records(app: &App, path: &str) -> Response {
    let reply = match app.daemon.request(Method::DELETE, path, None).await {
        Ok(reply) if reply.status.is_success() => reply,
        Ok(reply) => return refused(&reply.message()),
        Err(error) => return refused(&error.to_string()),
    };
    let removed: RemovedRecords = serde_json::from_slice(&reply.body).unwrap_or_default();
    let notice = views::notice(&crate::format::sentence(&removed.line()), Tone::Done);
    let records = app.daemon.get::<Records>("/v1/records").await;
    let status = app.daemon.get::<Status>("/v1/status").await;
    match (status, records) {
        (Ok(status), Ok(records)) => {
            html(&[caches::records(&status, &records, crate::now()), notice])
        }
        _ => html(&[notice]),
    }
}

/// The editor's signals.
#[derive(Debug, Deserialize)]
pub(crate) struct Edit {
    toml: String,
    /// The revision of the config the editor started from; a config replaced since is not overwritten.
    #[serde(default)]
    revision: Option<String>,
}

/// `POST /config/check`: the result region for the text in the editor.
pub(crate) async fn check_config(State(app): State<App>, Json(edit): Json<Edit>) -> Response {
    match app
        .daemon
        .post_toml::<ConfigCheck>("/v1/config/check", edit.toml.clone())
        .await
    {
        Ok(check) => html(&[config::result(&check, &edit.toml, &Outcome::Checked)]),
        Err(error) => refused(&error.to_string()),
    }
}

/// `PUT /config`: saves the text in the editor and has the daemon reload it, or says why the daemon would not.
pub(crate) async fn save_config(State(app): State<App>, Json(edit): Json<Edit>) -> Response {
    let path = match &edit.revision {
        Some(revision) => format!(
            "/v1/config?{}",
            serde_urlencoded::to_string([("revision", revision)]).unwrap_or_default()
        ),
        None => "/v1/config".to_owned(),
    };
    let reply = match app
        .daemon
        .request(Method::PUT, &path, Some(Body::toml(edit.toml.clone())))
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
            // The revision of the text saved, never a later read's, which may be of a config written since.
            let revision = check.revision.clone();
            html(&[config::result(
                &check,
                &edit.toml,
                &Outcome::Saved { path, revision },
            )])
        }
        (StatusCode::UNPROCESSABLE_ENTITY, Ok(check)) => {
            html(&[config::result(&check, &edit.toml, &Outcome::Refused)])
        }
        _ => refused(&format!("Not saved: {}", reply.message())),
    }
}
