//! The control routes: pause (forced or not), reseed, cache clean, the config file (show, check, replace and
//! reload) and the doctor. Every handler takes the authenticated principal. The engine carries out what changes
//! scheduling through `Control`; this module only writes the config file, atomically, after it validated it.

use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use runnerd_core::api::{Busy, ConfigCheck, ConfigFile, Doctor, PauseQuery};
use runnerd_core::config::Config;
use runnerd_core::scheduler::engine::ControlError;
use tracing::info;

use super::{AppState, Authenticated, MemoryMax, error};

fn refused(control_error: &ControlError) -> Response {
    let status = match control_error {
        ControlError::NotFound(_) => StatusCode::NOT_FOUND,
        ControlError::Busy { message, jobs } => {
            return (
                StatusCode::CONFLICT,
                Json(Busy {
                    message: message.clone(),
                    jobs: jobs.clone(),
                }),
            )
                .into_response();
        }
        ControlError::Conflict(_) => StatusCode::CONFLICT,
        ControlError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        ControlError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ControlError::Stopped => StatusCode::SERVICE_UNAVAILABLE,
    };
    error(status, &control_error.to_string())
}

pub(super) async fn pause(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
    Query(query): Query<PauseQuery>,
) -> Response {
    let repository = format!("{owner}/{name}");
    info!(
        "{repository}: pause (force: {}) requested by {principal:?}",
        query.force
    );
    match state.handle.control.pause(&repository, query.force).await {
        Ok(paused) => Json(paused).into_response(),
        Err(control_error) => refused(&control_error),
    }
}

pub(super) async fn reseed(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    UrlPath(profile): UrlPath<String>,
) -> Response {
    info!("{profile}: reseed requested by {principal:?}");
    match state.handle.control.reseed(&profile).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(control_error) => refused(&control_error),
    }
}

pub(super) async fn clean_cache(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    info!("{name}: clean requested by {principal:?}");
    match state.handle.control.clean_cache(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(control_error) => refused(&control_error),
    }
}

pub(super) async fn config(_: Authenticated, State(state): State<AppState>) -> Json<ConfigFile> {
    let file = state.config.lock().await;
    Json(ConfigFile {
        path: file.path.display().to_string(),
        text: file.text.clone(),
    })
}

/// Checks a config off the async threads: reading a slice's `MemoryMax` runs `systemctl`.
async fn check(
    memory_max: MemoryMax,
    text: String,
) -> Result<(Option<Config>, ConfigCheck), tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || Config::check(&text, memory_max)).await
}

fn failed(join_error: &tokio::task::JoinError) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, &join_error.to_string())
}

pub(super) async fn check_config(
    _: Authenticated,
    State(state): State<AppState>,
    text: String,
) -> Response {
    match check(state.memory_max, text).await {
        Ok((_, checked)) => Json(checked).into_response(),
        Err(join_error) => failed(&join_error),
    }
}

/// Validates the new config, writes it in place of the file and has the engine run with it. An invalid config
/// leaves the file and the engine as they were.
pub(super) async fn replace_config(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    text: String,
) -> Response {
    // One replacement at a time, so the file and the engine end up with the same config.
    let mut file = state.config.lock().await;
    let (config, mut checked) = match check(state.memory_max, text.clone()).await {
        Ok(checked) => checked,
        Err(join_error) => return failed(&join_error),
    };
    let Some(config) = config else {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(checked)).into_response();
    };
    let database = |config: &Config| config.history.database.clone();
    if Config::parse(&file.text).ok().as_ref().map(database) != Some(database(&config)) {
        checked
            .warnings
            .push("history.database takes effect when runnerd restarts".to_owned());
    }
    if let Err(io_error) = write_atomically(&file.path, &text) {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cannot write {}: {io_error}", file.path.display()),
        );
    }
    file.text = text;
    info!("{} replaced by {principal:?}", file.path.display());
    match state
        .handle
        .control
        .reload(config, checked.memory_max)
        .await
    {
        Ok(()) => Json(checked).into_response(),
        Err(control_error) => refused(&control_error),
    }
}

/// Replaces the file `path` points to, through a symlink if it is one, with `text`: a temporary file in the same
/// directory with the same permissions, renamed over it, so a reader sees the old file or the new one.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    let target = std::fs::canonicalize(path)?;
    let mode = std::fs::metadata(&target)?.permissions().mode();
    let dir = target.parent().unwrap_or(Path::new("/"));
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary: PathBuf = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        // `mode` passes through the umask.
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &target)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

pub(super) async fn doctor(_: Authenticated, State(state): State<AppState>) -> Response {
    let text = state.config.lock().await.text.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let config = Config::parse(&text).ok();
        let host = crate::doctor::Host::read(config.as_ref());
        crate::doctor::checks(&host, config.as_ref())
    })
    .await;
    match checked {
        Ok(checks) => Json(Doctor { checks }).into_response(),
        Err(join_error) => failed(&join_error),
    }
}
