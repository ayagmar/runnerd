//! `runnerd-web`: the dashboard the daemon serves on a loopback TCP port. It is a client of the daemon's API like
//! `runnerctl`: it reads `runnerd_core::api` types over the daemon's Unix socket (`runnerd_core::client`), renders
//! HTML on the server (`views`, `chart`), and keeps open pages current over SSE with Datastar (`live`). Writes
//! (pause, reseed, the config) are passed on to the same API (`actions`). It holds no state of its own beyond the
//! session and its login codes (`auth`), and never talks to Docker or GitHub.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use maud::Markup;
use runnerd_core::client::{Client, ClientError};
use runnerd_core::config::Origin;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

mod actions;
mod assets;
mod auth;
mod chart;
mod format;
mod live;
mod views;

pub use auth::{Sessions, login_file, session_file};
use views::caches::CachesView;
use views::config::{ConfigQuery, ConfigView};
use views::history::{Filter, History};
use views::job::JobView;
use views::overview::Overview;
use views::sizing::SizingView;

/// The dashboard's default address.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7477";

/// What every handler shares.
#[derive(Clone)]
pub(crate) struct App {
    daemon: Client,
    /// `[web] origins`, where the config page offers to sign a phone in.
    proxies: Arc<[String]>,
    /// Set when the dashboard stops, so live pages end.
    stopping: watch::Receiver<bool>,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Binds the dashboard's port from `runnerd serve --web`: a loopback address, or `off` for no dashboard. Only
/// loopback: the session cookie travels in the clear.
pub async fn listen(flag: &str) -> io::Result<Option<TcpListener>> {
    if flag == "off" {
        return Ok(None);
    }
    let addr: SocketAddr = flag
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the dashboard listens on loopback only, not {addr}"),
        ));
    }
    TcpListener::bind(addr).await.map(Some)
}

/// Runs `serve` as a task, logging why it stopped if it failed.
pub fn spawn(
    listener: TcpListener,
    socket: PathBuf,
    proxies: Vec<Origin>,
    sessions: Arc<Sessions>,
    stop: impl Future + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let stop = async {
            stop.await;
        };
        if let Err(error) = serve(listener, &socket, &proxies, sessions, stop).await {
            warn!("the dashboard stopped: {error}");
        }
    })
}

/// Serves the dashboard on `listener` for the daemon whose API is on `socket`, until `stop` completes. Pages from
/// `proxies` (`[web] origins`), reverse proxies in front of it, may write too. A browser signed in to `sessions`
/// is let in. Writes a one-time login link per address to `login_file(socket)` and removes it when done.
pub async fn serve(
    listener: TcpListener,
    socket: &Path,
    proxies: &[Origin],
    sessions: Arc<Sessions>,
    stop: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let addr = listener.local_addr()?;
    let file = login_file(socket);
    let auth = auth::Auth::new(addr, proxies, sessions, file.clone())?;
    info!(
        "serving the dashboard on http://{addr}; `runnerctl web` opens it (login links in {})",
        file.display()
    );
    let (stopping, stopping_receiver) = watch::channel(false);
    let app = router(
        Client::new(socket.to_owned()),
        auth.clone(),
        stopping_receiver,
    );
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop.await;
            let _ = stopping.send(true);
        })
        .await;
    auth.remove_login_file();
    served
}

fn router(daemon: Client, auth: Arc<auth::Auth>, stopping: watch::Receiver<bool>) -> Router {
    let proxies = auth.proxies().into();
    let pages = Router::new()
        .route("/", get(overview))
        .route("/history", get(history))
        .route("/caches", get(caches))
        .route("/sizing", get(sizing))
        .route("/jobs/{id}", get(job))
        .route("/jobs/{id}/log", get(job_log))
        .route("/live/overview", get(live::overview))
        .route("/live/jobs/{id}", get(live::job))
        .route("/config", get(config).put(actions::save_config))
        .route("/config/check", post(actions::check_config))
        .route(
            "/v1/repositories/{owner}/{name}/pause",
            post(actions::pause),
        )
        .route(
            "/v1/repositories/{owner}/{name}/resume",
            post(actions::resume),
        )
        .route("/v1/repositories/pause", post(actions::pause_all))
        .route("/v1/repositories/resume", post(actions::resume_all))
        .route("/v1/profiles/{name}/reseed", post(actions::reseed))
        .route("/v1/caches/{name}/clean", post(actions::clean))
        .route("/v1/records", delete(actions::remove_all_records))
        .route("/v1/records/{job}", delete(actions::remove_record))
        .route("/assets/{name}", get(assets::serve))
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                views::error_page("Not found", "There is no page at this address."),
            )
        })
        .with_state(App {
            daemon,
            proxies,
            stopping,
        });
    let login = Router::new()
        .route("/login", get(auth::login))
        .route("/pair", post(auth::pair))
        .with_state(auth.clone());
    pages
        .merge(login)
        .layer(middleware::from_fn_with_state(auth, auth::require_session))
        .layer(middleware::from_fn(security_headers))
}

/// No framing, no sniffing, no referrer, and a policy that lets the page load only what this server serves.
/// Datastar evaluates its attributes with `Function`, hence `'unsafe-eval'`; regions size their blocks with
/// `style` attributes.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self'; connect-src 'self'; manifest-src 'self'; form-action 'self'; \
             base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// A page, or the daemon's error as one.
fn page(rendered: Result<Markup, ClientError>) -> Response {
    match rendered {
        Ok(markup) => Html(markup.into_string()).into_response(),
        Err(ClientError::Api {
            status: StatusCode::NOT_FOUND,
            message,
            ..
        }) => (
            StatusCode::NOT_FOUND,
            views::error_page("Not found", &message),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            views::error_page("runnerd did not answer", &error.to_string()),
        )
            .into_response(),
    }
}

async fn overview(State(app): State<App>) -> Response {
    page(
        Overview::load(&app.daemon, now())
            .await
            .map(|overview| views::overview::render(&overview)),
    )
}

async fn history(State(app): State<App>, Query(filter): Query<Filter>) -> Response {
    page(
        History::load(&app.daemon, filter, now())
            .await
            .map(|history| views::history::render(&history)),
    )
}

async fn caches(State(app): State<App>) -> Response {
    page(
        CachesView::load(&app.daemon, now())
            .await
            .map(|caches| views::caches::render(&caches)),
    )
}

async fn sizing(State(app): State<App>) -> Response {
    page(
        SizingView::load(&app.daemon)
            .await
            .map(|sizing| views::sizing::render(&sizing)),
    )
}

async fn config(State(app): State<App>, Query(query): Query<ConfigQuery>) -> Response {
    page(
        ConfigView::load(&app.daemon, query)
            .await
            .map(|config| views::config::render(&config, &app.proxies)),
    )
}

async fn job(State(app): State<App>, UrlPath(id): UrlPath<u64>) -> Response {
    page(
        JobView::load(&app.daemon, id, now())
            .await
            .map(|job| views::job::render(&job)),
    )
}

/// `GET /jobs/{id}/log`: the whole stored log as text.
async fn job_log(State(app): State<App>, UrlPath(id): UrlPath<u64>) -> Response {
    match app.daemon.text(&format!("/v1/jobs/{id}/logs")).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        Err(ClientError::Api {
            status, message, ..
        }) => (status, message).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests;
