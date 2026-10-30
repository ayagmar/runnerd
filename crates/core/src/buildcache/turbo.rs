//! Turborepo's remote cache API on a runner's socket (ADR 0083), besides its artifacts, which `serve` reads and
//! writes as any tool's entries under `<owner>/<name>/turbo/<hash>`: the status turbo asks at the start of a run, the
//! events it reports, and the lookup of many artifacts at once that a dry run makes. None of them reads or writes an
//! entry. A request with a token other than the image's is a workflow's own, for Vercel's remote cache: it passes it
//! to Vercel's API as it came, and Vercel's answer back, and to no other origin. It must not log a request's headers,
//! which carry turbo's token.

use std::io;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_LENGTH};
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use tracing::debug;

use super::actions::{json, passed_headers, read};
use super::serve::{Body, drain, status};
use super::{Inner, valid_segment};
use crate::sync::lock;

/// Where turbo's API is on the socket: the image's `TURBO_API` ends in `/turbo`, and every path turbo asks is under
/// `v8/artifacts`.
pub(super) const API: &str = "turbo/v8/artifacts";

/// The tool its artifacts are entries of, under the repository's directory.
pub(super) const TOOL: &str = "turbo";

/// What it answers under `API`: everything else there is an artifact's.
pub(super) const ENDPOINTS: [&str; 3] = ["", "/status", "/events"];

/// The token the image gives turbo (`TURBO_TOKEN`), which only turns turbo's remote cache on.
const TOKEN: &str = "runnerd";

/// Where a request with a workflow's own token goes: turbo's own default API, Vercel's. A fixed origin, with a path of
/// its own, so the token in the request reaches Vercel or nothing, whatever the request's path.
const VERCEL_API: &str = "https://vercel.com/api";

/// How long a request passed to Vercel may wait for its connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a request passed to Vercel may wait for the next bytes of its answer. No limit on the whole: an artifact
/// may take minutes.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest lookup it reads: tens of thousands of tasks' hashes. turbo looks a larger one up one hash at a time.
const MAX_QUERY: usize = 1 << 20;

/// Answers one of `ENDPOINTS`, `endpoint`, for a runner of `repository`.
pub(super) async fn handle(
    inner: &Inner,
    repository: &str,
    endpoint: &str,
    request: Request<Incoming>,
) -> Response<Body> {
    match (endpoint, request.method()) {
        ("", &Method::POST) => query(inner, repository, request.into_body()).await,
        // Writable whatever the job: an upload not kept is answered as stored, as every tool's is (`serve`).
        ("/status", &Method::GET) => json(&serde_json::json!({ "status": "enabled" })),
        // What turbo reports of its hits and misses: the socket counts them itself.
        ("/events", &Method::POST) => {
            drain(request.into_body()).await;
            status(StatusCode::OK)
        }
        _ => status(StatusCode::METHOD_NOT_ALLOWED),
    }
}

#[derive(Deserialize)]
struct Query {
    hashes: Vec<String>,
}

/// Which of the hashes asked are artifacts of `repository`'s: each hash maps to its hit, or to `null`. A hit is not
/// counted, as a `HEAD` is not, and says no time saved, which runnerd does not keep.
async fn query(inner: &Inner, repository: &str, body: Incoming) -> Response<Body> {
    let Some(query) = read(body, MAX_QUERY)
        .await
        .and_then(|body| serde_json::from_slice::<Query>(&body).ok())
    else {
        return status(StatusCode::BAD_REQUEST);
    };
    let state = lock(&inner.state);
    let hits: serde_json::Map<String, serde_json::Value> = query
        .hashes
        .into_iter()
        .map(|hash| {
            let known = valid_segment(&hash)
                && state
                    .entries
                    .contains_key(&Path::new(repository).join(TOOL).join(&hash));
            let hit = if known {
                serde_json::json!({ "taskDurationMs": 0 })
            } else {
                serde_json::Value::Null
            };
            (hash, hit)
        })
        .collect();
    drop(state);
    json(&serde_json::Value::Object(hits))
}

/// Whether `request` carries a bearer token other than the image's: the workflow set its own `TURBO_TOKEN`, as for
/// Vercel's remote cache, and none of runnerd's entries are for it.
pub(super) fn foreign(request: &Request<Incoming>) -> bool {
    request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("bearer") && token.trim() != TOKEN
        })
}

/// Vercel's API, which the requests with a workflow's own token go to.
#[derive(Debug)]
pub(super) struct Vercel {
    origin: Mutex<String>,
    client: OnceLock<Option<reqwest::Client>>,
}

impl Vercel {
    pub(super) fn new() -> Vercel {
        Vercel {
            origin: Mutex::new(VERCEL_API.to_owned()),
            client: OnceLock::new(),
        }
    }

    /// Sends what it passes on to `origin` instead of Vercel's API.
    #[cfg(test)]
    pub(super) fn set_origin(&self, origin: &str) {
        *lock(&self.origin) = origin.to_owned();
    }

    fn client(&self) -> Option<&reqwest::Client> {
        self.client
            .get_or_init(|| {
                crate::github::install_crypto_provider();
                reqwest::Client::builder()
                    .connect_timeout(CONNECT_TIMEOUT)
                    .read_timeout(READ_TIMEOUT)
                    // A redirect goes back to turbo, which follows it itself.
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|error| debug!("cannot make Vercel's client: {error}"))
                    .ok()
            })
            .as_ref()
    }
}

/// Passes `request`, whose path is under `/turbo`, to Vercel's API as it came, its body as it arrives, with the path
/// after `/turbo` and its query; and Vercel's answer back the same way. Neither is read, counted or kept.
pub(super) async fn pass(inner: &Inner, request: Request<Incoming>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let Some(client) = inner.vercel.client() else {
        return status(StatusCode::BAD_GATEWAY);
    };
    let path = parts.uri.path_and_query().map_or("", |path| path.as_str());
    let url = format!(
        "{}{}",
        lock(&inner.vercel.origin),
        path.strip_prefix("/turbo").unwrap_or_default()
    );
    // The upload's length, when turbo sent one, goes with its body: hyper sends it from the body's size.
    let sent = client
        .request(parts.method, url)
        .headers(passed_headers(&parts.headers))
        .body(reqwest::Body::wrap(body))
        .send()
        .await;
    let answer = match sent {
        Ok(answer) => answer,
        Err(error) => {
            debug!(
                "cannot pass a request to Vercel's API: {}",
                error.without_url()
            );
            return status(StatusCode::BAD_GATEWAY);
        }
    };
    let code = answer.status();
    let mut headers = passed_headers(answer.headers());
    if let Some(length) = answer.headers().get(CONTENT_LENGTH) {
        headers.insert(CONTENT_LENGTH, length.clone());
    }
    let chunks = futures_util::stream::unfold(Some(answer), |answer| async move {
        let mut answer = answer?;
        match answer.chunk().await {
            Ok(Some(chunk)) => Some((Ok(Frame::data(chunk)), Some(answer))),
            Ok(None) => None,
            Err(error) => Some((Err(io::Error::other(error.without_url())), None)),
        }
    });
    let mut response = Response::new(BodyExt::boxed_unsync(StreamBody::new(chunks)));
    *response.status_mut() = code;
    *response.headers_mut() = headers;
    response
}
