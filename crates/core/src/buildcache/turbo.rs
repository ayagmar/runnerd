//! Turborepo's remote cache API on a runner's socket (ADR 0083), besides its artifacts, which `serve` reads and
//! writes as any tool's entries under `<owner>/<name>/turbo/<hash>`: the status turbo asks at the start of a run, the
//! events it reports, and the lookup of many artifacts at once that a dry run makes. None of them reads or writes an
//! entry. It must not log a request's headers, which carry turbo's token.

use std::path::Path;

use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;

use super::actions::{json, read};
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
