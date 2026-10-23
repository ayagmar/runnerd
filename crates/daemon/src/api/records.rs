//! The records routes: list and remove the records of failed jobs, and open a shell in one. A shell's terminal is
//! the request's own connection, upgraded to `api::SHELL_PROTOCOL`: no other listener. The engine starts and removes
//! the shell's container through `Control`; this module only carries the terminal's bytes, and asks for the
//! container's removal once either side closes, however the connection ends.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hyper_util::rt::TokioIo;
use runnerd_core::api::{RemoveRecordsQuery, SHELL_PROTOCOL, ShellQuery};
use runnerd_core::config::parse_duration;
use runnerd_core::docker::{Tty, TtySize};
use runnerd_core::record;
use runnerd_core::scheduler::engine::{Control, RecordSelection};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use super::control::refused;
use super::{AppState, Authenticated, error};

/// The most log lines a shell reads to find what its step ran.
const LOG_LINES: u64 = 200_000;

pub(super) async fn list(_: Authenticated, State(state): State<AppState>) -> Response {
    Json(state.handle.records.borrow().clone()).into_response()
}

pub(super) async fn remove(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Path(job): Path<u64>,
) -> Response {
    info!("removal of job {job}'s record requested by {principal:?}");
    match state
        .handle
        .control
        .remove_records(RecordSelection::Job(job))
        .await
    {
        Ok(removed) => Json(removed).into_response(),
        Err(control_error) => refused(&control_error),
    }
}

pub(super) async fn remove_many(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Query(query): Query<RemoveRecordsQuery>,
) -> Response {
    let selection = match (query.all, query.older_than.as_deref()) {
        (true, None) => RecordSelection::All,
        (false, Some(age)) => match parse_duration(age) {
            Ok(age) => RecordSelection::OlderThan(age),
            Err(message) => return error(StatusCode::BAD_REQUEST, &message),
        },
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "say which records: all=true or older_than, such as older_than=2d",
            );
        }
    };
    info!("removal of records ({selection:?}) requested by {principal:?}");
    match state.handle.control.remove_records(selection).await {
        Ok(removed) => Json(removed).into_response(),
        Err(control_error) => refused(&control_error),
    }
}

pub(super) async fn shell(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Path(job): Path<u64>,
    Query(query): Query<ShellQuery>,
    mut request: Request,
) -> Response {
    let upgrade = request.headers().get(header::UPGRADE);
    if upgrade.is_none_or(|protocol| protocol != SHELL_PROTOCOL) {
        return error(
            StatusCode::UPGRADE_REQUIRED,
            &format!(
                "a shell is a terminal: send Connection: Upgrade and Upgrade: {SHELL_PROTOCOL}"
            ),
        );
    }
    let banner = match banner(&state, job, query.step).await {
        Ok(banner) => banner,
        Err(response) => return *response,
    };
    let size = TtySize {
        cols: query.cols.unwrap_or(80),
        rows: query.rows.unwrap_or(24),
    };
    let control = state.handle.control.clone();
    let tty = match control.open_shell(job, size).await {
        Ok(tty) => tty,
        Err(control_error) => return refused(&control_error),
    };
    info!("shell in the record of job {job} opened by {principal:?}");
    let close = Close {
        control,
        container: tty.container.clone(),
    };
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        match upgraded.await {
            Ok(upgraded) => carry(TokioIo::new(upgraded), tty, banner).await,
            Err(upgrade_error) => warn!("the shell of job {job} lost its client: {upgrade_error}"),
        }
        drop(close);
    });
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static(SHELL_PROTOCOL));
    response
}

/// What the shell prints first, from the job's record in the history.
async fn banner(state: &AppState, job: u64, step: Option<u32>) -> Result<String, Box<Response>> {
    let store = &state.handle.store;
    let read_failed = |store_error: &dyn std::fmt::Display| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &store_error.to_string(),
        ))
    };
    let Some(found) = store.job(job).await.map_err(|e| read_failed(&e))? else {
        return Err(Box::new(error(
            StatusCode::NOT_FOUND,
            &format!("job {job} is not in the job history"),
        )));
    };
    let log: Vec<String> = store
        .log_lines(job, 0, LOG_LINES)
        .await
        .map_err(|e| read_failed(&e))?
        .into_iter()
        .map(|(_, line)| line)
        .collect();
    Ok(record::banner(&found, &log, step))
}

/// Carries a terminal's bytes between the client and the shell until either ends.
async fn carry(client: TokioIo<hyper::upgrade::Upgraded>, tty: Tty, banner: String) {
    let (mut from_client, mut to_client) = tokio::io::split(client);
    let Tty {
        mut output,
        mut input,
        ..
    } = tty;
    let shown = async {
        to_client.write_all(banner.as_bytes()).await?;
        while let Some(Ok(bytes)) = output.next().await {
            to_client.write_all(&bytes).await?;
            to_client.flush().await?;
        }
        to_client.shutdown().await
    };
    tokio::pin!(shown);
    tokio::select! {
        _ = &mut shown => return,
        _ = tokio::io::copy(&mut from_client, &mut input) => {}
    }
    // The client sent all it will, or went away: the shell's input ends, which ends a shell waiting for it, and what
    // it still prints goes to a client that stayed.
    drop(input);
    let _ = tokio::time::timeout(DRAIN, shown).await;
}

/// How long a shell's output is still carried once its client sent all it will.
const DRAIN: Duration = Duration::from_secs(30);

/// Removes a shell's container when dropped: when its terminal ends, or the task carrying it does.
struct Close {
    control: Arc<Control>,
    container: String,
}

impl Drop for Close {
    fn drop(&mut self) {
        self.control
            .close_shell(std::mem::take(&mut self.container));
    }
}
