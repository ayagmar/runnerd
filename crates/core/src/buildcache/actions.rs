//! GitHub Actions' cache on a runner's socket (ADR 0082): the cache service v2 that `actions/cache`, the `setup-*`
//! actions and BuildKit's `type=gha` speak, Twirp in JSON with Azure's block blob protocol for the archives, served
//! from the build cache's entries under `<owner>/<name>/actions`. An entry is scoped to a branch: a job writes only
//! into its run's branch, and only when the engine said it may write; it reads that branch's entries, then the
//! default branch's. Every other request of GitHub's results service, such as an artifact's, goes to GitHub as it
//! came. It must not log a request's headers or body, which carry the job's token.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use http_body_util::{BodyExt, Empty, Full, Limited, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HOST, HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::debug;

use super::serve::{Body, Receiving, drain, receive, status};
use super::{ActionsCache, Inner, Runner, Upload, Writes, charge, valid_segment};
use crate::sync::lock;

/// The paths it answers, under the socket's root: GitHub's Twirp services, and the archives' uploads and downloads,
/// whose first segment Azure's clients read as a storage account's name, as on Azure's emulator.
pub(super) const TWIRP: &str = "twirp/";
pub(super) const BLOBS: &str = "runnerdcache/";

/// GitHub's cache service, which it answers itself.
const CACHE_SERVICE: &str = "twirp/github.actions.results.api.v1.CacheService/";

const UPLOADS: &str = "runnerdcache/upload/";
const DOWNLOADS: &str = "runnerdcache/download/";

/// Where every other Twirp request goes: GitHub's results service, the `ACTIONS_RESULTS_URL` the runner gives jobs.
const RESULTS_ORIGIN: &str = "https://results-receiver.actions.githubusercontent.com";

/// The entries' tool directory in a repository's: the build cache's tools never take this name.
const TOOL: &str = "actions";

/// The segment after a branch's in an entry's path, and the start of an entry's file name, which no hex has.
const END: &str = "~";

/// The hex digits of a branch's or a key's name in one path segment: a long key takes several.
const SEGMENT: usize = 128;

/// How long a save waits for the engine to decide whether its job may write, before it is refused.
const DECISION_WAIT: Duration = Duration::from_secs(60);

/// The longest key `@actions/cache` sends.
const MAX_KEY: usize = 512;

/// The largest cache service request it reads, and block list.
const MAX_REQUEST: usize = 64 << 10;
const MAX_BLOCK_LIST: usize = 4 << 20;

/// The largest request and answer it passes between a job and GitHub's results service.
const MAX_PASSED: usize = 1 << 20;

/// How long GitHub's results service may take to answer a request it passes on.
const PASS_TIMEOUT: Duration = Duration::from_secs(60);

/// The saves one runner may have open at once, the blocks one save may have, and the entries its lookups may find:
/// past them, saves are refused and lookups miss.
const MAX_SAVES: usize = 1_000;
const MAX_BLOCKS: usize = 50_000;
const MAX_FOUND: usize = 10_000;

/// The address in the URLs it answers when a request names none: the image's forwarder.
const DEFAULT_HOST: &str = "127.0.0.1:41772";

/// Azure's clients, BuildKit's among them, expect a request ID in each answer.
const REQUEST_ID: HeaderName = HeaderName::from_static("x-ms-request-id");

/// The range Azure's clients ask for: theirs, or the standard one.
const MS_RANGE: HeaderName = HeaderName::from_static("x-ms-range");

/// A save being uploaded: an entry of the job's branch, once its archive is in and its job finalizes it.
#[derive(Debug)]
pub(super) struct Save {
    /// The entry's path in the entries' directory, but for its last segment, which says when it was created.
    dir: PathBuf,
    key: String,
    version: String,
    /// Its blocks so far, by their ID, until a block list joins them.
    blocks: HashMap<String, Upload>,
    /// The archive, sent whole or joined from blocks.
    archive: Option<Upload>,
    /// What it sent passed a limit and was dropped: its finalize answers as stored.
    dropped: bool,
}

impl Save {
    /// What of it is on disk, to be removed.
    pub(super) fn into_uploads(self) -> Vec<Upload> {
        self.blocks.into_values().chain(self.archive).collect()
    }
}

/// The client that passes requests to GitHub's results service, made when the first is passed.
#[derive(Debug)]
pub(super) struct Results {
    origin: Mutex<String>,
    client: OnceLock<Option<reqwest::Client>>,
}

impl Results {
    pub(super) fn new() -> Results {
        Results {
            origin: Mutex::new(RESULTS_ORIGIN.to_owned()),
            client: OnceLock::new(),
        }
    }

    /// Sends what it passes on to `origin` instead of GitHub's.
    #[cfg(test)]
    pub(super) fn set_origin(&self, origin: &str) {
        *lock(&self.origin) = origin.to_owned();
    }

    fn client(&self) -> Option<&reqwest::Client> {
        self.client
            .get_or_init(|| {
                crate::github::install_crypto_provider();
                reqwest::Client::builder()
                    .timeout(PASS_TIMEOUT)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|error| debug!("cannot make the results service's client: {error}"))
                    .ok()
            })
            .as_ref()
    }
}

pub(super) async fn handle(
    inner: &Inner,
    runner: &str,
    request: Request<Incoming>,
) -> Response<Body> {
    let served = lock(&inner.state)
        .runners
        .get(runner)
        .is_some_and(|open| open.actions == ActionsCache::Served);
    if !served {
        return status(StatusCode::NOT_FOUND);
    }
    let path = request.uri().path().trim_start_matches('/').to_owned();
    if let Some(method) = path.strip_prefix(CACHE_SERVICE) {
        if request.method() != Method::POST {
            return twirp_error(StatusCode::NOT_FOUND, "bad_route");
        }
        let host = host(request.headers());
        let Some(body) = read(request.into_body(), MAX_REQUEST).await else {
            return twirp_error(StatusCode::BAD_REQUEST, "malformed");
        };
        return match method {
            "GetCacheEntryDownloadURL" => lookup(inner, runner, &host, &body),
            "CreateCacheEntry" => create(inner, runner, &host, &body).await,
            "FinalizeCacheEntryUpload" => finalize(inner, runner, &body),
            _ => twirp_error(StatusCode::NOT_FOUND, "bad_route"),
        };
    }
    if path.starts_with(TWIRP) {
        return pass(inner, request).await;
    }
    if let Some(id) = path.strip_prefix(UPLOADS) {
        let id = id.parse().unwrap_or(0);
        return upload(inner, runner, id, request).await;
    }
    if let Some(id) = path.strip_prefix(DOWNLOADS)
        && let Ok(id) = id.parse()
    {
        return download(inner, runner, id, &request).await;
    }
    status(StatusCode::NOT_FOUND)
}

#[derive(Debug, Deserialize)]
struct LookupRequest {
    key: String,
    #[serde(default, alias = "restoreKeys")]
    restore_keys: Vec<String>,
    version: String,
}

#[derive(Debug, Deserialize)]
struct CreateRequest {
    key: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct FinalizeRequest {
    key: String,
    version: String,
    /// A string from `@actions/cache` (an int64 in protobuf's JSON), a number from BuildKit.
    #[serde(default, alias = "sizeBytes")]
    size_bytes: serde_json::Value,
}

/// `GetCacheEntryDownloadURL`: the entry the key or a restore key names in the job's scopes, as GitHub matches
/// them: in each scope in order, each key exactly, then as a prefix, the newest created first.
fn lookup(inner: &Inner, runner: &str, host: &str, body: &[u8]) -> Response<Body> {
    let Ok(request) = serde_json::from_slice::<LookupRequest>(body) else {
        return twirp_error(StatusCode::BAD_REQUEST, "malformed");
    };
    let mut state = lock(&inner.state);
    let Some(open) = state.runners.get(runner) else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let scopes = read_scopes(open, state.default_branches.get(&open.repository));
    let keys: Vec<&str> = std::iter::once(request.key.as_str())
        .chain(request.restore_keys.iter().map(String::as_str))
        .filter(|key| valid_key(key))
        .collect();
    let found = if valid_segment(&request.version) {
        scopes.iter().find_map(|branch| {
            let scope = scope_dir(&open.repository, branch, &request.version);
            keys.iter()
                .find_map(|key| newest(state.entries.keys(), &scope, key))
        })
    } else {
        None
    };
    let Some(open) = state.runners.get_mut(runner) else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let found = found.and_then(|(path, key)| {
        let id = match open.found.iter().position(|known| *known == path) {
            Some(id) => id,
            None if open.found.len() < MAX_FOUND => {
                open.found.push(path.clone());
                open.found.len() - 1
            }
            None => return None,
        };
        Some((path, key, id))
    });
    let Some((path, key, id)) = found else {
        open.counts.misses += 1;
        return json(&serde_json::json!({
            "ok": false,
            "signed_download_url": "",
            "matched_key": "",
        }));
    };
    open.counts.hits += 1;
    drop(state);
    inner.touch(&path);
    json(&serde_json::json!({
        "ok": true,
        "signed_download_url": format!("http://{host}/{DOWNLOADS}{id}"),
        "matched_key": key,
    }))
}

/// The branches whose entries a job reads, in order: its run's when it may write, then the default branch, as its
/// decision said or, while undecided, as the last decision of its repository did.
fn read_scopes(open: &Runner, default_branch: Option<&String>) -> Vec<String> {
    let mut scopes = Vec::new();
    if open.writes == Writes::Allowed
        && let Some(run) = &open.branches.run
    {
        scopes.push(run.clone());
    }
    if let Some(default) = open.branches.default.as_ref().or(default_branch)
        && !scopes.contains(default)
    {
        scopes.push(default.clone());
    }
    scopes
}

/// Of `entries`, the newest created in `scope` whose key is `key`, else the newest whose key starts with it.
fn newest<'entries>(
    entries: impl Iterator<Item = &'entries PathBuf>,
    scope: &Path,
    key: &str,
) -> Option<(PathBuf, String)> {
    let mut exact: Option<(u64, &PathBuf, String)> = None;
    let mut prefixed: Option<(u64, &PathBuf, String)> = None;
    for path in entries {
        let Some((found, created)) = parse_entry(path, scope) else {
            continue;
        };
        let best = if found == key {
            &mut exact
        } else if found.starts_with(key) {
            &mut prefixed
        } else {
            continue;
        };
        if best.as_ref().is_none_or(|(newest, _, _)| created > *newest) {
            *best = Some((created, path, found));
        }
    }
    exact.or(prefixed).map(|(_, path, key)| (path.clone(), key))
}

/// `CreateCacheEntry`: an upload URL for a save into the job's branch. A job that may not write, or whose writes
/// are still undecided after `DECISION_WAIT`, gets one whose archive is read and dropped, as `actions/cache` and
/// BuildKit fail a build whose save is refused. A key its branch has already is refused, as GitHub refuses it.
async fn create(inner: &Inner, runner: &str, host: &str, body: &[u8]) -> Response<Body> {
    let Ok(request) = serde_json::from_slice::<CreateRequest>(body) else {
        return twirp_error(StatusCode::BAD_REQUEST, "malformed");
    };
    if !valid_key(&request.key) || !valid_segment(&request.version) {
        return twirp_error(StatusCode::BAD_REQUEST, "invalid_argument");
    }
    wait_for_decision(inner, runner).await;
    let mut state = lock(&inner.state);
    let Some(open) = state.runners.get(runner) else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let branch = match (&open.writes, &open.branches.run) {
        (Writes::Allowed, Some(branch)) if open.saves.len() < MAX_SAVES => Some(branch.clone()),
        _ => None,
    };
    let Some(branch) = branch else {
        if let Some(open) = state.runners.get_mut(runner) {
            open.counts.refused += 1;
        }
        // No save has number 0: what is sent to it is dropped.
        return created(host, 0);
    };
    let dir = entry_dir(&open.repository, &branch, &request.version, &request.key);
    let taken = state
        .entries
        .keys()
        .any(|path| path.parent() == Some(dir.as_path()))
        || open.saves.values().any(|save| save.dir == dir);
    if taken {
        return json(&serde_json::json!({ "ok": false, "signed_upload_url": "", "message": "" }));
    }
    state.next_upload += 1;
    let id = state.next_upload;
    if let Some(open) = state.runners.get_mut(runner) {
        open.saves.insert(
            id,
            Save {
                dir,
                key: request.key,
                version: request.version,
                blocks: HashMap::new(),
                archive: None,
                dropped: false,
            },
        );
    }
    created(host, id)
}

fn created(host: &str, id: u64) -> Response<Body> {
    json(&serde_json::json!({
        "ok": true,
        "signed_upload_url": format!("http://{host}/{UPLOADS}{id}"),
        "message": "",
    }))
}

/// Waits until the engine has decided whether `runner`'s job may write, at most `DECISION_WAIT`.
async fn wait_for_decision(inner: &Inner, runner: &str) {
    let deadline = tokio::time::Instant::now() + DECISION_WAIT;
    loop {
        let decided = match lock(&inner.state).runners.get(runner) {
            Some(open) if open.writes == Writes::Undecided => open.decided.clone(),
            _ => return,
        };
        let notified = decided.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        // Decided between the check and `enable`.
        if lock(&inner.state)
            .runners
            .get(runner)
            .is_none_or(|open| open.writes != Writes::Undecided)
        {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return;
        }
    }
}

/// `FinalizeCacheEntryUpload`: the save's archive becomes an entry of its branch, created now. One whose archive was
/// dropped, or a job's that may not write, answers as stored.
fn finalize(inner: &Inner, runner: &str, body: &[u8]) -> Response<Body> {
    let Ok(request) = serde_json::from_slice::<FinalizeRequest>(body) else {
        return twirp_error(StatusCode::BAD_REQUEST, "malformed");
    };
    let size = match &request.size_bytes {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    };
    let stored = |entry_id: u64| {
        json(&serde_json::json!({ "ok": true, "entry_id": entry_id.to_string(), "message": "" }))
    };
    let not_stored = || json(&serde_json::json!({ "ok": false, "entry_id": "0", "message": "" }));
    let mut state = lock(&inner.state);
    let created = crate::time::now_ms().max(state.last_created + 1);
    let Some(open) = state.runners.get_mut(runner) else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let id = open
        .saves
        .iter()
        .filter(|(_, save)| save.key == request.key && save.version == request.version)
        .find(|(_, save)| save.archive.is_some() || save.dropped)
        .map(|(id, _)| *id);
    let Some(save) = id.and_then(|id| open.saves.remove(&id)) else {
        return if open.writes == Writes::Allowed {
            not_stored()
        } else {
            stored(0)
        };
    };
    let Save {
        dir,
        blocks,
        archive,
        dropped,
        ..
    } = save;
    let leftover: Vec<Upload> = blocks.into_values().collect();
    let archive = archive.filter(|_| !dropped);
    let fits = archive
        .as_ref()
        .is_some_and(|archive| size.is_none_or(|size| size == archive.bytes));
    drop(state);
    inner.remove_uploads(Some(runner), &leftover);
    match archive {
        Some(mut archive) if fits => {
            {
                let mut state = lock(&inner.state);
                state.last_created = state.last_created.max(created);
            }
            archive.entry = dir.join(format!("{END}{created}"));
            inner.store(runner, archive);
            stored(created)
        }
        Some(archive) => {
            inner.remove_uploads(Some(runner), &[archive]);
            not_stored()
        }
        None => stored(0),
    }
}

/// The archive of save `id`: sent whole (Azure's Put Blob), in blocks (Put Block), or joined from them (Put Block
/// List). What a save no longer open, or past a limit, sends is read and dropped, and answered as stored.
async fn upload(
    inner: &Inner,
    runner: &str,
    id: u64,
    request: Request<Incoming>,
) -> Response<Body> {
    if request.method() != Method::PUT {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let query: Vec<(String, String)> = request
        .uri()
        .query()
        .and_then(|query| serde_urlencoded::from_str(query).ok())
        .unwrap_or_default();
    let field = |name: &str| {
        query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let (comp, block) = (field("comp"), field("blockid"));
    let (open, max_size) = {
        let state = lock(&inner.state);
        let open = state
            .runners
            .get(runner)
            .and_then(|open| open.saves.get(&id))
            .is_some_and(|save| !save.dropped);
        (open, state.max_size)
    };
    let body = request.into_body();
    if !open {
        drain(body).await;
        return blob_created();
    }
    match (comp.as_deref(), block) {
        (Some("blocklist"), _) => join(inner, runner, id, body).await,
        (Some("block"), Some(block)) if !block.is_empty() && block.len() <= 128 => {
            let Some(received) = take(inner, runner, max_size, body).await else {
                return status(StatusCode::INTERNAL_SERVER_ERROR);
            };
            keep(inner, runner, id, received, Part::Block(block));
            blob_created()
        }
        (Some("block"), _) => status(StatusCode::BAD_REQUEST),
        _ => {
            let Some(received) = take(inner, runner, max_size, body).await else {
                return status(StatusCode::INTERNAL_SERVER_ERROR);
            };
            keep(inner, runner, id, received, Part::Archive);
            blob_created()
        }
    }
}

/// Receives a body into an upload file: `None` when writing it failed, `Some(None)` when it passed a limit.
async fn take(
    inner: &Inner,
    runner: &str,
    max_size: u64,
    body: Incoming,
) -> Option<Option<Upload>> {
    let mut receiving = Receiving::new(inner, runner, max_size);
    match receive(&mut receiving, body).await {
        Ok(true) => Some(Some(receiving.into_upload(PathBuf::new()))),
        Ok(false) => Some(None),
        Err(error) => {
            debug!("{runner}: an actions cache upload failed: {error}");
            None
        }
    }
}

enum Part {
    Block(String),
    Archive,
}

/// Keeps what save `id` was sent, or, when it passed a limit, drops the save's archive for good. An upload of a
/// save no longer open goes.
fn keep(inner: &Inner, runner: &str, id: u64, received: Option<Upload>, part: Part) {
    let mut gone = Vec::new();
    {
        let mut state = lock(&inner.state);
        let Some(open) = state.runners.get_mut(runner) else {
            drop(state);
            inner.remove_uploads(None, &received.into_iter().collect::<Vec<_>>());
            return;
        };
        let Some(save) = open.saves.get_mut(&id).filter(|save| !save.dropped) else {
            gone.extend(received);
            drop(state);
            inner.remove_uploads(Some(runner), &gone);
            return;
        };
        match (received, part) {
            (Some(upload), Part::Block(block))
                if save.blocks.len() < MAX_BLOCKS || save.blocks.contains_key(&block) =>
            {
                gone.extend(save.blocks.insert(block, upload));
            }
            (Some(upload), Part::Archive) => gone.extend(save.archive.replace(upload)),
            (received, _) => {
                gone.extend(received);
                gone.extend(save.blocks.drain().map(|(_, upload)| upload));
                gone.extend(save.archive.take());
                save.dropped = true;
                open.counts.refused += 1;
            }
        }
    }
    inner.remove_uploads(Some(runner), &gone);
}

/// Put Block List: joins save `id`'s blocks in the list's order into its archive; blocks the list leaves out go.
async fn join(inner: &Inner, runner: &str, id: u64, body: Incoming) -> Response<Body> {
    let Some(list) = read(body, MAX_BLOCK_LIST).await else {
        return status(StatusCode::BAD_REQUEST);
    };
    let Some(order) = block_ids(&String::from_utf8_lossy(&list)) else {
        return status(StatusCode::BAD_REQUEST);
    };
    let blocks = {
        let mut state = lock(&inner.state);
        let Some(save) = state
            .runners
            .get_mut(runner)
            .and_then(|open| open.saves.get_mut(&id))
        else {
            return blob_created();
        };
        std::mem::take(&mut save.blocks)
    };
    let mut blocks = blocks;
    let mut parts = Vec::with_capacity(order.len());
    for block in &order {
        let Some(upload) = blocks.remove(block) else {
            // Azure answers a list naming a block it lacks with 400, and so does this, dropping the save.
            parts.extend(blocks.into_values());
            inner.remove_uploads(Some(runner), &parts);
            keep(inner, runner, id, None, Part::Archive);
            return status(StatusCode::BAD_REQUEST);
        };
        parts.push(upload);
    }
    inner.remove_uploads(Some(runner), &blocks.into_values().collect::<Vec<_>>());
    let file = {
        let mut state = lock(&inner.state);
        state.next_upload += 1;
        inner.uploads.join(state.next_upload.to_string())
    };
    let sources: Vec<PathBuf> = parts.iter().map(|part| part.file.clone()).collect();
    let target = file.clone();
    let joined = tokio::task::spawn_blocking(move || -> io::Result<u64> {
        let mut out = std::fs::File::create(&target)?;
        let mut bytes = 0;
        for source in sources {
            bytes += io::copy(&mut std::fs::File::open(source)?, &mut out)?;
        }
        out.sync_data()?;
        Ok(bytes)
    })
    .await;
    let room: u64 = parts.iter().map(|part| part.room).sum();
    // The blocks' files go; their room stays with the archive, which needs no more than they took together.
    for part in &parts {
        let _ = std::fs::remove_file(&part.file);
    }
    let Ok(Ok(bytes)) = joined else {
        let _ = std::fs::remove_file(&file);
        inner.release(Some(runner), room);
        keep(inner, runner, id, None, Part::Archive);
        return status(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let needed = charge(bytes);
    inner.release(Some(runner), room.saturating_sub(needed));
    let archive = Upload {
        file,
        entry: PathBuf::new(),
        bytes,
        room: room.min(needed),
    };
    keep(inner, runner, id, Some(archive), Part::Archive);
    blob_created()
}

/// The block IDs of an Azure block list, in order: `<Latest>`, `<Committed>` and `<Uncommitted>` alike.
fn block_ids(list: &str) -> Option<Vec<String>> {
    let mut ids = Vec::new();
    let mut rest = list;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let end = rest.find('>')?;
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if matches!(tag, "Latest" | "Committed" | "Uncommitted") {
            let close = rest.find("</")?;
            ids.push(rest[..close].trim().to_owned());
            rest = &rest[close..];
        }
    }
    Some(ids)
}

/// An entry a lookup of the job's found: whole, or the range Azure's clients ask for.
async fn download(
    inner: &Inner,
    runner: &str,
    id: usize,
    request: &Request<Incoming>,
) -> Response<Body> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some(entry) = lock(&inner.state)
        .runners
        .get(runner)
        .and_then(|open| open.found.get(id).cloned())
    else {
        return status(StatusCode::NOT_FOUND);
    };
    let Ok(mut file) = tokio::fs::File::open(inner.root.join(&entry)).await else {
        return status(StatusCode::NOT_FOUND);
    };
    let Ok(size) = file.metadata().await.map(|metadata| metadata.len()) else {
        return status(StatusCode::NOT_FOUND);
    };
    let range = request
        .headers()
        .get(MS_RANGE)
        .or_else(|| request.headers().get(hyper::header::RANGE))
        .and_then(|value| value.to_str().ok())
        .and_then(parse_range);
    let (start, length, partial) = match range {
        None => (0, size, false),
        Some((start, _)) if start >= size => {
            let mut response = status(StatusCode::RANGE_NOT_SATISFIABLE);
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(CONTENT_RANGE, value);
            }
            return response;
        }
        Some((start, end)) => {
            let end = end.map_or(size - 1, |end| end.min(size - 1));
            (start, end - start + 1, true)
        }
    };
    let head = request.method() == Method::HEAD;
    if !head && file.seek(io::SeekFrom::Start(start)).await.is_err() {
        return status(StatusCode::INTERNAL_SERVER_ERROR);
    }
    if !head && let Some(open) = lock(&inner.state).runners.get_mut(runner) {
        open.counts.read_bytes += length;
    }
    let body = if head {
        Empty::new().map_err(|never| match never {}).boxed_unsync()
    } else {
        let chunks = futures_util::stream::unfold((file, length), |(mut file, left)| async move {
            if left == 0 {
                return None;
            }
            let mut chunk = vec![0; (64 << 10).min(usize::try_from(left).unwrap_or(usize::MAX))];
            match file.read(&mut chunk).await {
                Ok(0) => None,
                Ok(read) => {
                    chunk.truncate(read);
                    Some((
                        Ok(Frame::data(Bytes::from(chunk))),
                        (file, left - read as u64),
                    ))
                }
                Err(error) => Some((Err(error), (file, 0))),
            }
        });
        BodyExt::boxed_unsync(StreamBody::new(chunks))
    };
    let mut response = Response::new(body);
    if partial {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(value) =
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + length - 1))
        {
            response.headers_mut().insert(CONTENT_RANGE, value);
        }
    }
    let headers = response.headers_mut();
    headers.insert(CONTENT_LENGTH, length.into());
    headers.insert(
        hyper::header::ACCEPT_RANGES,
        HeaderValue::from_static("bytes"),
    );
    headers.insert(REQUEST_ID, request_id());
    response
}

/// One range, `bytes=<start>-` or `bytes=<start>-<end>`; anything else is served whole.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => None,
        end => Some(end.parse().ok()?),
    };
    end.is_none_or(|end| end >= start).then_some((start, end))
}

/// Passes a request for another of GitHub's results services, such as an artifact's, to GitHub as it came, and
/// GitHub's answer back. Its headers carry the job's token: none is logged, nor the URL's query.
async fn pass(inner: &Inner, request: Request<Incoming>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let Some(body) = read(body, MAX_PASSED).await else {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let Some(client) = inner.results.client() else {
        return status(StatusCode::BAD_GATEWAY);
    };
    let url = format!(
        "{}{}",
        lock(&inner.results.origin),
        parts.uri.path_and_query().map_or("/", |path| path.as_str())
    );
    let sent = client
        .request(parts.method, url)
        .headers(passed_headers(&parts.headers))
        .body(body)
        .send()
        .await;
    let mut answer = match sent {
        Ok(answer) => answer,
        Err(error) => {
            debug!(
                "cannot pass {} to GitHub's results service: {}",
                parts.uri.path(),
                error.without_url()
            );
            return status(StatusCode::BAD_GATEWAY);
        }
    };
    let code = answer.status();
    let headers = passed_headers(answer.headers());
    let mut bytes = Vec::new();
    loop {
        match answer.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= MAX_PASSED => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            _ => return status(StatusCode::BAD_GATEWAY),
        }
    }
    let mut response = Response::new(
        Full::new(Bytes::from(bytes))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = code;
    *response.headers_mut() = headers;
    response
}

/// The headers passed on either way: all but those of the connection, which each side sets for itself.
fn passed_headers(headers: &HeaderMap) -> HeaderMap {
    const OWN: [&str; 10] = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
    ];
    headers
        .iter()
        .filter(|(name, _)| !OWN.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// The address a request came to, for the URLs it answers: the forwarder's, the same for a node action and for
/// BuildKit in a container of the slot's engine.
fn host(headers: &HeaderMap) -> String {
    headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|host| {
            !host.is_empty()
                && host.len() <= 64
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".:-[]".contains(&byte))
        })
        .unwrap_or(DEFAULT_HOST)
        .to_owned()
}

/// Whether `key` is one `@actions/cache` sends: not empty, at most `MAX_KEY` bytes, no comma.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY && !key.contains(',')
}

/// Where a branch's entries of a version are: `<owner>/<name>/actions/<branch>/~/<version>`, the branch in hex.
fn scope_dir(repository: &str, branch: &str, version: &str) -> PathBuf {
    let mut path = PathBuf::from(repository);
    path.push(TOOL);
    push_hex(&mut path, branch);
    path.push(END);
    path.push(version);
    path
}

/// Where an entry of `key` is, but for its last segment, `~<created>`.
fn entry_dir(repository: &str, branch: &str, version: &str, key: &str) -> PathBuf {
    let mut path = scope_dir(repository, branch, version);
    push_hex(&mut path, key);
    path
}

/// Pushes `text` in hex, `SEGMENT` digits a segment: whatever a job names stays in its own directory.
fn push_hex(path: &mut PathBuf, text: &str) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let hex: String = text
        .bytes()
        .flat_map(|byte| {
            [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 15)],
            ]
        })
        .map(char::from)
        .collect();
    for segment in hex.as_bytes().chunks(SEGMENT) {
        path.push(String::from_utf8_lossy(segment).as_ref());
    }
}

/// The key and creation time of the entry at `path`, if it is one of `scope`'s.
fn parse_entry(path: &Path, scope: &Path) -> Option<(String, u64)> {
    let rest = path.strip_prefix(scope).ok()?;
    let segments: Vec<&str> = rest
        .iter()
        .map(|segment| segment.to_str())
        .collect::<Option<_>>()?;
    let (last, hex) = segments.split_last()?;
    let created = last.strip_prefix(END)?.parse().ok()?;
    let hex = hex.concat();
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some((String::from_utf8(bytes).ok()?, created))
}

/// Reads a body of at most `limit` bytes.
async fn read(body: Incoming, limit: usize) -> Option<Bytes> {
    Limited::new(body, limit)
        .collect()
        .await
        .ok()
        .map(http_body_util::Collected::to_bytes)
}

fn json(value: &serde_json::Value) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(value.to_string()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// A Twirp error, as `@actions/cache` and BuildKit read one.
fn twirp_error(code: StatusCode, kind: &str) -> Response<Body> {
    let mut response = json(&serde_json::json!({ "code": kind, "msg": kind }));
    *response.status_mut() = code;
    response
}

/// What Azure answers a block, a block list or a whole archive with.
fn blob_created() -> Response<Body> {
    let mut response = status(StatusCode::CREATED);
    response.headers_mut().insert(REQUEST_ID, request_id());
    response
}

fn request_id() -> HeaderValue {
    HeaderValue::from_static("00000000-0000-0000-0000-000000000000")
}

#[cfg(test)]
mod tests;

/// An answer of a runner's socket: its status, headers (names in lower case) and body.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

#[cfg(test)]
impl Answer {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }
}

/// Sends one request with `headers` to a runner's socket in `dir`, as a job's node action or BuildKit does through
/// the image's forwarder, which they reach at 10.0.0.5:41772.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "a test helper: a socket that does not answer fails the test"
)]
pub(crate) async fn send(
    dir: &Path,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Answer {
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::UnixStream::connect(dir.join(super::SOCKET))
        .await
        .unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 10.0.0.5:41772\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.extend([name, ": ", value, "\r\n"]);
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines.next().unwrap()[9..12].parse().unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Answer {
        status,
        headers,
        body: response[split + 4..].to_vec(),
    }
}

/// Calls a method of the cache service with `request`, as `@actions/cache` does.
#[cfg(test)]
pub(crate) async fn call(
    dir: &Path,
    method: &str,
    request: &serde_json::Value,
) -> serde_json::Value {
    send(
        dir,
        "POST",
        &format!("/{CACHE_SERVICE}{method}"),
        &[
            ("Authorization", "Bearer runtime-token"),
            ("Content-Type", "application/json"),
        ],
        request.to_string().as_bytes(),
    )
    .await
    .json()
}

/// The path of a URL the cache answered with.
#[cfg(test)]
pub(crate) fn path_of(url: &serde_json::Value) -> String {
    let url = url.as_str().unwrap_or_default();
    url.strip_prefix("http://10.0.0.5:41772")
        .unwrap_or(url)
        .to_owned()
}

/// Saves `archive` under `key` as `@actions/cache` does: reserves, sends it whole, finalizes. True when each step
/// answered as GitHub's service does to a save it keeps, which it also does to one it drops.
#[cfg(test)]
pub(crate) async fn save(dir: &Path, key: &str, archive: &[u8]) -> bool {
    let reserved = call(
        dir,
        "CreateCacheEntry",
        &serde_json::json!({ "key": key, "version": VERSION }),
    )
    .await;
    if reserved["ok"] != true {
        return false;
    }
    let sent = send(
        dir,
        "PUT",
        &path_of(&reserved["signed_upload_url"]),
        &[("x-ms-blob-type", "BlockBlob")],
        archive,
    )
    .await;
    let finalized = call(
        dir,
        "FinalizeCacheEntryUpload",
        &serde_json::json!({ "key": key, "version": VERSION, "size_bytes": archive.len().to_string() }),
    )
    .await;
    sent.status == 201 && finalized["ok"] == true
}

/// Restores as `@actions/cache` does: the key it matched and the archive, `None` on a miss.
#[cfg(test)]
pub(crate) async fn restore(
    dir: &Path,
    key: &str,
    restore_keys: &[&str],
) -> Option<(String, Vec<u8>)> {
    let found = call(
        dir,
        "GetCacheEntryDownloadURL",
        &serde_json::json!({ "key": key, "restore_keys": restore_keys, "version": VERSION }),
    )
    .await;
    if found["ok"] != true {
        return None;
    }
    let archive = send(
        dir,
        "GET",
        &path_of(&found["signed_download_url"]),
        &[],
        b"",
    )
    .await;
    assert_eq!(archive.status, 200, "{archive:?}");
    Some((found["matched_key"].as_str()?.to_owned(), archive.body))
}

/// The version `@actions/cache` sends for one set of paths and compression.
#[cfg(test)]
pub(crate) const VERSION: &str = "b4f2e8a1c3d5e7f9a1b3c5d7e9f1a3b5c7d9e1f3a5b7c9d1e3f5a7b9c1d3e5f7";
