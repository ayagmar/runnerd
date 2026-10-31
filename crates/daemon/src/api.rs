//! Serves `runnerd_core::api` on the local Unix socket. Owns routing, the socket file and authentication: every
//! handler takes a `Principal` from the one extractor here, so other transports only have to produce one. It holds
//! no scheduling state of its own and returns no secret but a pairing link. History reads are in `history`, the event stream in
//! `events`, pause, resume, reseed, cache clean, the config file and the doctor in `control`, records and the shells
//! in them in `records`. `auto` applies sizing advice under `[sizing] auto` through `control`'s config path; `notify` sends
//! `[notify]`'s notifications and never returns a target's URL or headers. Signing
//! every browser out of the dashboard and making its pairing links go through `runnerd_web::Sessions`, which owns
//! them; a pairing link is the one secret the API returns, to the owner only.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, FromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::serve::IncomingStream;
use axum::{Json, Router};
use runnerd_core::api::{
    Dashboard, ErrorResponse, Pairing, PairingQuery, Pairings, Principal, ShutdownRequest, Status,
};
use runnerd_core::config::{ByteSize, Config};
use runnerd_core::scheduler::engine::{Handle, Shutdown};
use runnerd_web::{PAIRING_LIFETIME, PAIRING_MAX, PairError, Sessions};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, watch};
use tracing::info;

mod auto;
mod control;
mod events;
mod history;
mod notify;
mod records;

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

/// Reads a slice's `MemoryMax`: systemd's (`config::slice_memory_max`), or a test's.
pub(crate) type MemoryMax = fn(&str) -> io::Result<Option<ByteSize>>;

/// Reads a slice's CPUs in millicores: systemd's (`config::slice_cpus`), or a test's.
pub(crate) type SliceCpus = fn(&str) -> io::Result<u64>;

/// Flushes a directory's entries to disk: `control::sync_dir`, or a test's.
pub(crate) type SyncDir = fn(&Path) -> io::Result<()>;

pub(crate) use control::sync_dir;

/// The config file the daemon runs with, which the config routes show and replace.
pub(crate) struct ConfigSource {
    pub path: PathBuf,
    /// As the daemon read it.
    pub text: String,
    pub memory_max: MemoryMax,
    /// Read when a new config is applied: a config that sets `cpu_overcommit` needs them, others show them.
    pub cpus: SliceCpus,
    pub sync_dir: SyncDir,
    /// How often auto sizing looks at the advice: `auto::EVERY`, or a test's.
    pub auto_every: std::time::Duration,
    /// How the notifier paces itself and reads the local time: `notify::SETUP`, or a test's.
    pub notify: notify::Setup,
}

pub(crate) use auto::EVERY as AUTO_SIZING_EVERY;
pub(crate) use notify::SETUP as NOTIFY;

/// The config file as last read or written.
struct ConfigText {
    path: PathBuf,
    text: String,
}

#[derive(Clone)]
struct AppState {
    handle: Handle,
    /// The user the daemon runs as: the only one the socket admits.
    owner: u32,
    /// Set when the server stops, so streams that would never end do.
    stopping: watch::Receiver<bool>,
    config: Arc<Mutex<ConfigText>>,
    /// Signalled each time `config` is replaced, for the notifier.
    config_replaced: Arc<tokio::sync::Notify>,
    memory_max: MemoryMax,
    cpus: SliceCpus,
    sync_dir: SyncDir,
    /// The dashboard's session secret.
    sessions: Arc<Sessions>,
    /// `[web]` as the config read at start said, which the dashboard runs with until a restart.
    dashboard: Dashboard,
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

/// The API's routes.
fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/repositories/pause", post(control::pause_all))
        .route("/v1/repositories/resume", post(control::resume_all))
        .route(
            "/v1/repositories/{owner}/{name}/pause",
            post(control::pause),
        )
        .route(
            "/v1/repositories/{owner}/{name}/resume",
            post(control::resume),
        )
        .route("/v1/profiles/{name}/reseed", post(control::reseed))
        .route("/v1/caches/{name}/clean", post(control::clean_cache))
        .route("/v1/config", get(control::config))
        .route("/v1/config", put(control::replace_config))
        .route("/v1/config/check", post(control::check_config))
        .route("/v1/doctor", get(control::doctor))
        .route("/v1/shutdown", post(shutdown))
        .route("/v1/dashboard", get(dashboard))
        .route("/v1/dashboard/sign-out", post(sign_out_dashboard))
        .route("/v1/dashboard/pairings", post(pair_dashboard))
        .route("/v1/jobs", get(history::jobs))
        .route("/v1/jobs/{id}", get(history::job))
        .route("/v1/jobs/{id}/samples", get(history::samples))
        .route("/v1/jobs/{id}/logs", get(history::logs))
        .route("/v1/host/samples", get(history::host_samples))
        .route("/v1/stats", get(history::stats))
        .route("/v1/sizing", get(history::sizing))
        .route("/v1/activity", get(history::activity))
        .route("/v1/insights", get(history::insights))
        .route("/v1/digest", get(history::digest))
        .route("/v1/savings", get(history::savings))
        .route("/v1/notify/test", post(notify::test))
        .route("/v1/caches", get(history::caches))
        .route("/v1/caches/history", get(history::cache_history))
        .route("/v1/events", get(events::events))
        .route("/v1/records", get(records::list))
        .route("/v1/records", delete(records::remove_many))
        .route("/v1/records/{job}", delete(records::remove))
        .route("/v1/records/{job}/shell", post(records::shell))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "no such endpoint") })
        .with_state(state)
}

async fn status(_: Authenticated, State(state): State<AppState>) -> Json<Status> {
    Json(state.handle.status.borrow().clone())
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
    state.handle.control.shutdown(if request.force {
        Shutdown::Force
    } else {
        Shutdown::Graceful
    });
    StatusCode::ACCEPTED
}

/// `GET /v1/dashboard`: the dashboard's `[web]` settings as the daemon started with them.
async fn dashboard(_: Authenticated, State(state): State<AppState>) -> Json<Dashboard> {
    Json(state.dashboard)
}

/// `POST /v1/dashboard/sign-out`: signs every browser out of the dashboard, with a new session secret.
async fn sign_out_dashboard(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
) -> Response {
    match state.sessions.sign_out_all() {
        Ok(()) => {
            info!("every browser signed out of the dashboard by {principal:?}");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(failure) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cannot write a new dashboard session secret: {failure}"),
        ),
    }
}

/// `POST /v1/dashboard/pairings?origin=`: a fresh link on `origin`, or on each of `[web] origins`, that signs one
/// device in there within 5 minutes, as `runnerctl web --qr` shows. More origins than codes may be alive at once is
/// refused, so every link returned works.
async fn pair_dashboard(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Query(query): Query<PairingQuery>,
) -> Response {
    let refused = |failure: &PairError| {
        let status = match failure {
            PairError::DashboardOff | PairError::NoOrigins => StatusCode::CONFLICT,
            PairError::NotListed(_) => StatusCode::BAD_REQUEST,
            PairError::SignedOut => StatusCode::UNAUTHORIZED,
            PairError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error(status, &failure.to_string())
    };
    let origins = match (query.origin, state.sessions.origins()) {
        (Some(origin), _) => vec![origin],
        (None, None) => return refused(&PairError::DashboardOff),
        (None, Some(origins)) if origins.is_empty() => return refused(&PairError::NoOrigins),
        (None, Some(origins)) => origins,
    };
    // Each new code would drop the oldest beyond the cap: refuse before making any rather than return dead links.
    if origins.len() > PAIRING_MAX {
        return error(
            StatusCode::CONFLICT,
            &format!(
                "[web] origins lists {} addresses, more than the {PAIRING_MAX} pairing codes alive at once; pick \
                 one with `runnerctl web --qr --base <origin>`",
                origins.len()
            ),
        );
    }
    let now = std::time::Instant::now();
    let mut links = Vec::new();
    for origin in origins {
        match state.sessions.pair(&origin, now) {
            Ok(url) => links.push(Pairing {
                origin,
                url,
                lifetime: PAIRING_LIFETIME.as_secs(),
            }),
            Err(failure) => return refused(&failure),
        }
    }
    info!(
        "{} dashboard pairing link(s) made for {principal:?}",
        links.len()
    );
    Json(Pairings { links }).into_response()
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
    config: ConfigSource,
    sessions: Arc<Sessions>,
    stop: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let owner = std::fs::metadata("/proc/self")?.uid();
    serve_for(listener, handle, config, sessions, owner, stop).await
}

/// Serves the API to the user `owner` only.
async fn serve_for(
    listener: UnixListener,
    handle: Handle,
    config: ConfigSource,
    sessions: Arc<Sessions>,
    owner: u32,
    stop: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let (stopping, stopping_receiver) = watch::channel(false);
    let dashboard = Dashboard {
        open: Config::parse(&config.text)
            .ok()
            .and_then(|parsed| parsed.web.open)
            .map(|open| open.to_string()),
    };
    let state = AppState {
        handle,
        owner,
        stopping: stopping_receiver,
        config: Arc::new(Mutex::new(ConfigText {
            path: config.path,
            text: config.text,
        })),
        config_replaced: Arc::default(),
        memory_max: config.memory_max,
        cpus: config.cpus,
        sync_dir: config.sync_dir,
        sessions,
        dashboard,
    };
    tokio::spawn(auto::run(state.clone(), config.auto_every));
    tokio::spawn(notify::run(notify::Notifier {
        store: state.handle.store.clone(),
        config: state.config.clone(),
        replaced: state.config_replaced.clone(),
        stopping: state.stopping.clone(),
        setup: config.notify,
    }));
    let app = router(state).into_make_service_with_connect_info::<Peer>();
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
