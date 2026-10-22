//! Serves `runnerd_core::api` on the local Unix socket. Owns routing, the socket file and authentication: every
//! handler takes a `Principal` from the one extractor here, so other transports only have to produce one. It holds
//! no scheduling state of its own and never returns secrets. History reads are in `history`, the event stream in
//! `events`.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, FromRequestParts, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::IncomingStream;
use axum::{Json, Router};
use runnerd_core::api::{ErrorResponse, Principal, ShutdownRequest, Status};
use runnerd_core::scheduler::engine::Handle;
use tokio::net::UnixListener;
use tokio::sync::watch;
use tracing::info;

mod events;
mod history;

/// The socket's default path: `$XDG_RUNTIME_DIR/runnerd.sock`.
pub(crate) fn default_socket() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| Path::new(&dir).join("runnerd.sock"))
}

/// The peer's user, from the socket's credentials.
#[derive(Debug, Clone, Copy)]
struct Peer {
    uid: Option<u32>,
}

impl Connected<IncomingStream<'_, UnixListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        Peer {
            uid: stream.io().peer_cred().ok().map(|cred| cred.uid()),
        }
    }
}

#[derive(Clone)]
struct AppState {
    handle: Handle,
    /// The user the daemon runs as: the only one the socket admits.
    owner: u32,
    /// Set when the server stops, so streams that would never end do.
    stopping: watch::Receiver<bool>,
}

/// The authenticated sender of a request.
struct Authenticated(Principal);

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<Peer>>()
            .map(|info| info.0);
        match peer.and_then(|peer| peer.uid) {
            Some(uid) if uid == state.owner => Ok(Authenticated(Principal::LocalOwner { uid })),
            _ => Err(error(
                StatusCode::FORBIDDEN,
                "only the user runnerd runs as may use its socket",
            )),
        }
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            message: message.to_owned(),
        }),
    )
        .into_response()
}

/// The API's routes, for a daemon running as `owner`.
fn router(handle: Handle, owner: u32, stopping: watch::Receiver<bool>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/repositories/{owner}/{name}/pause", post(pause))
        .route("/v1/repositories/{owner}/{name}/resume", post(resume))
        .route("/v1/shutdown", post(shutdown))
        .route("/v1/jobs", get(history::jobs))
        .route("/v1/jobs/{id}", get(history::job))
        .route("/v1/jobs/{id}/samples", get(history::samples))
        .route("/v1/jobs/{id}/logs", get(history::logs))
        .route("/v1/host/samples", get(history::host_samples))
        .route("/v1/stats", get(history::stats))
        .route("/v1/caches", get(history::caches))
        .route("/v1/caches/history", get(history::cache_history))
        .route("/v1/events", get(events::events))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "no such endpoint") })
        .with_state(AppState {
            handle,
            owner,
            stopping,
        })
}

async fn status(_: Authenticated, State(state): State<AppState>) -> Json<Status> {
    Json(state.handle.status.borrow().clone())
}

async fn pause(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
) -> Response {
    set_paused(&principal, &state, &format!("{owner}/{name}"), true)
}

async fn resume(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
) -> Response {
    set_paused(&principal, &state, &format!("{owner}/{name}"), false)
}

fn set_paused(principal: &Principal, state: &AppState, repository: &str, paused: bool) -> Response {
    if state.handle.control.set_paused(repository, paused) {
        info!(
            "{repository} {} by {principal:?}",
            if paused { "paused" } else { "resumed" }
        );
        StatusCode::NO_CONTENT.into_response()
    } else {
        error(
            StatusCode::NOT_FOUND,
            &format!("{repository} is not a configured repository"),
        )
    }
}

async fn shutdown(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    request: Option<Json<ShutdownRequest>>,
) -> StatusCode {
    let Json(request) = request.unwrap_or_default();
    info!(
        "shutdown (force: {}) requested by {principal:?}",
        request.force
    );
    state.handle.control.shutdown(request.force);
    StatusCode::ACCEPTED
}

/// Binds the socket with mode 0600. A socket file no daemon answers on is replaced; a live one is an error.
pub(crate) async fn bind(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{} is in use: runnerd runs already", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves the API on `listener` until `stop` completes.
pub(crate) async fn serve(
    listener: UnixListener,
    handle: Handle,
    stop: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let owner = std::fs::metadata("/proc/self")?.uid();
    serve_for(listener, handle, owner, stop).await
}

/// Serves the API to the user `owner` only.
async fn serve_for(
    listener: UnixListener,
    handle: Handle,
    owner: u32,
    stop: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let (stopping, stopping_receiver) = watch::channel(false);
    let app =
        router(handle, owner, stopping_receiver).into_make_service_with_connect_info::<Peer>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop.await;
            // Ends the event and log streams, which the graceful shutdown would otherwise wait for.
            let _ = stopping.send(true);
        })
        .await
}

#[cfg(test)]
mod tests;
