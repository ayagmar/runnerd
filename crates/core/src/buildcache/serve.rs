//! The build cache's HTTP on a runner's socket: each tool's paths mapped to entries of the runner's repository, the
//! lookups and uploads counted for that runner. Every path a tool may use is an entry's; nothing reads or writes
//! outside the repository's entries. It must not log a request's headers, which may carry a tool's token.

use std::convert::Infallible;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::CONTENT_LENGTH;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tracing::debug;

use super::{Inner, MAX_HELD, Upload, Writes, charge, valid_segment};
use crate::sync::lock;

pub(super) type Body = UnsyncBoxBody<Bytes, io::Error>;

/// The tools whose requests it answers, each with the path of its API before its keys: `/<tool>/<api><key>`. Each
/// reads with `GET`, a 404 being a miss, and writes with `PUT`; Bazel's keys are `ac/<sha256>` and `cas/<sha256>`,
/// sccache's `<a>/<b>/<c>/<key>`, Turborepo's and Nx's a task's hash. An entry's path leaves out the API's:
/// `<tool>/<key>`.
const TOOLS: [(&str, &str); 5] = [
    ("gradle", ""),
    ("sccache", ""),
    ("bazel", ""),
    (super::turbo::TOOL, "v8/artifacts/"),
    ("nx", "v1/cache/"),
];

/// Path segments a key may have at most: sccache's take four.
const MAX_SEGMENTS: usize = 8;

/// How much of an entry a read sends at a time.
const CHUNK: usize = 64 << 10;

/// Serves one connection of `runner`'s until either end closes it.
pub(super) async fn connection(inner: Arc<Inner>, runner: String, stream: UnixStream) {
    let service = service_fn(move |request| {
        let inner = inner.clone();
        let runner = runner.clone();
        async move { Ok::<_, Infallible>(handle(&inner, &runner, request).await) }
    });
    if let Err(error) = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        debug!("a build cache connection ended: {error}");
    }
}

async fn handle(inner: &Arc<Inner>, runner: &str, request: Request<Incoming>) -> Response<Body> {
    let Some(repository) = lock(&inner.state)
        .runners
        .get(runner)
        .map(|open| open.repository.clone())
    else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let path = request.uri().path().trim_start_matches('/').to_owned();
    if path.starts_with(super::actions::TWIRP) || path.starts_with(super::actions::BLOBS) {
        return super::actions::handle(inner, runner, request).await;
    }
    if (path == super::turbo::TOOL || path.starts_with("turbo/")) && super::turbo::foreign(&request)
    {
        return super::turbo::pass(inner, request).await;
    }
    if let Some(endpoint) = path.strip_prefix(super::turbo::API)
        && super::turbo::ENDPOINTS.contains(&endpoint)
    {
        return super::turbo::handle(inner, &repository, endpoint, request).await;
    }
    let Some((tool, rest)) = path.split_once('/').and_then(|(name, rest)| {
        let (tool, api) = TOOLS.iter().find(|(tool, _)| *tool == name)?;
        Some((*tool, rest.strip_prefix(api)?))
    }) else {
        return status(StatusCode::NOT_FOUND);
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.len() > MAX_SEGMENTS || !segments.iter().all(|segment| valid_segment(segment)) {
        return status(StatusCode::NOT_FOUND);
    }
    let mut entry = PathBuf::from(&repository);
    entry.push(tool);
    entry.extend(&segments);
    // A tool's own probe, such as sccache's `.sccache_check`, is not a lookup of a build's.
    let counted = !segments.last().is_some_and(|last| last.starts_with('.'));
    match *request.method() {
        Method::GET => read(inner, runner, entry, counted, ReadKind::Body).await,
        Method::HEAD => read(inner, runner, entry, false, ReadKind::Head).await,
        Method::PUT => write(inner, runner, entry, request.into_body()).await,
        _ => status(StatusCode::METHOD_NOT_ALLOWED),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadKind {
    Body,
    Head,
}

async fn read(
    inner: &Inner,
    runner: &str,
    entry: PathBuf,
    counted: bool,
    kind: ReadKind,
) -> Response<Body> {
    let known = lock(&inner.state).entries.contains_key(&entry);
    let file = if known {
        tokio::fs::File::open(inner.root.join(&entry)).await.ok()
    } else {
        None
    };
    // The file's own length: an upload stored meanwhile may have replaced the entry the index knew.
    let opened = match file {
        Some(file) => file
            .metadata()
            .await
            .ok()
            .map(|metadata| (file, metadata.len())),
        None => None,
    };
    let Some((file, bytes)) = opened else {
        if counted && let Some(open) = lock(&inner.state).runners.get_mut(runner) {
            open.counts.misses += 1;
        }
        return status(StatusCode::NOT_FOUND);
    };
    if counted {
        if let Some(open) = lock(&inner.state).runners.get_mut(runner) {
            open.counts.hits += 1;
            open.counts.read_bytes += bytes;
        }
        inner.touch(&entry);
    }
    let body = match kind {
        ReadKind::Head => Empty::new().map_err(|never| match never {}).boxed_unsync(),
        ReadKind::Body => {
            let chunks = futures_util::stream::unfold(file, |mut file| async move {
                let mut chunk = vec![0; CHUNK];
                match file.read(&mut chunk).await {
                    Ok(0) => None,
                    Ok(read) => {
                        chunk.truncate(read);
                        Some((Ok(Frame::data(Bytes::from(chunk))), file))
                    }
                    Err(error) => Some((Err(error), file)),
                }
            });
            BodyExt::boxed_unsync(StreamBody::new(chunks))
        }
    };
    let mut response = Response::new(body);
    response.headers_mut().insert(CONTENT_LENGTH, bytes.into());
    response
}

async fn write(inner: &Inner, runner: &str, entry: PathBuf, body: Incoming) -> Response<Body> {
    use hyper::body::Body as _;

    let (writes, max_size) = {
        let state = lock(&inner.state);
        let writes = state.runners.get(runner).map(|open| open.writes);
        (writes, state.max_size)
    };
    let refused = || {
        if let Some(open) = lock(&inner.state).runners.get_mut(runner) {
            open.counts.refused += 1;
        }
        accepted()
    };
    // Bazel sends every blob of its outputs again, having no way to ask which the cache has.
    let length = body.size_hint().exact();
    let same = length.is_some_and(|length| {
        lock(&inner.state)
            .entries
            .get(&entry)
            .is_some_and(|known| known.bytes == length)
    });
    if same && writes == Some(Writes::Allowed) {
        drain(body).await;
        // Still in use, though only ever sent again: Bazel's blobs an action's result needs.
        inner.touch(&entry);
        return accepted();
    }
    match writes {
        None => return status(StatusCode::SERVICE_UNAVAILABLE),
        // Read in full and dropped: a tool whose upload is cut off may stop using the cache for the rest of the
        // build.
        Some(Writes::Refused) => {
            drain(body).await;
            return refused();
        }
        Some(Writes::Allowed | Writes::Undecided) => {}
    }
    let mut receiving = Receiving::new(inner, runner, max_size, Purpose::Build);
    match receive(&mut receiving, body).await {
        Ok(true) => {}
        Ok(false) => return refused(),
        Err(error) => {
            debug!("{runner}: a build cache upload failed: {error}");
            return status(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    let upload = receiving.into_upload(entry);
    let writes = {
        let mut state = lock(&inner.state);
        match state.runners.get_mut(runner) {
            Some(open) if open.writes == Writes::Undecided && open.held.len() < MAX_HELD => {
                open.held.push(upload);
                return accepted();
            }
            Some(open) if open.writes == Writes::Undecided => None,
            Some(open) => Some(open.writes),
            None => None,
        }
    };
    if writes == Some(Writes::Allowed) {
        // Renames: quick, and on the daemon's own file system.
        inner.store(runner, upload);
        accepted()
    } else {
        inner.remove_uploads(Some(runner), &[upload]);
        refused()
    }
}

/// What an upload is for, which says whether its runner's decision still lets it in.
#[derive(Debug, Clone, Copy)]
pub(super) enum Purpose {
    /// A tool's entry: kept unless the job may not write.
    Build,
    /// A block or the archive of GitHub Actions cache save `id`: kept while the save takes uploads.
    Save(u64),
}

/// An upload being received. Unless it becomes an `Upload`, its file goes and the uploads' room it took is given
/// back, also when its connection is cut midway, as when its runner is removed.
pub(super) struct Receiving<'inner> {
    inner: &'inner Inner,
    runner: &'inner str,
    file: PathBuf,
    bytes: u64,
    /// The uploads' room it took: what its bytes count for (`charge`).
    room: u64,
    /// The size limit when it started.
    max_size: u64,
    purpose: Purpose,
    /// Changes when its runner's writes are decided; `None` when its runner is not open.
    decided: Option<watch::Receiver<()>>,
}

impl<'inner> Receiving<'inner> {
    /// An upload of `runner`'s for `purpose` to a new file of the uploads' directory, under the size limit `max_size`
    /// it started with.
    pub(super) fn new(
        inner: &'inner Inner,
        runner: &'inner str,
        max_size: u64,
        purpose: Purpose,
    ) -> Receiving<'inner> {
        let (file, decided) = {
            let mut state = lock(&inner.state);
            state.next_upload += 1;
            let decided = state
                .runners
                .get(runner)
                .map(|open| open.decided.subscribe());
            (inner.uploads.join(state.next_upload.to_string()), decided)
        };
        Receiving {
            inner,
            runner,
            file,
            bytes: 0,
            room: 0,
            max_size,
            purpose,
            decided,
        }
    }

    /// Whether its runner's decision, as far as it is made, still lets it in.
    fn wanted(&self) -> bool {
        lock(&self.inner.state)
            .runners
            .get(self.runner)
            .is_some_and(|open| match self.purpose {
                Purpose::Build => open.writes != Writes::Refused,
                Purpose::Save(id) => super::actions::takes_uploads(&open.saves, id),
            })
    }

    /// Takes the room `length` more bytes need, unless the upload would pass a quarter of the size limit, as no entry
    /// may, or the uploads' room runs out.
    fn grow(&mut self, length: u64) -> bool {
        let bytes = self.bytes + length;
        let more = charge(bytes).saturating_sub(self.room);
        if bytes > self.max_size / 4
            || more > 0 && !self.inner.reserve(self.runner, more, self.max_size)
        {
            return false;
        }
        self.bytes = bytes;
        self.room += more;
        true
    }

    /// Removes its file and gives back its room now, before the rest of its body is read and dropped: a sender that
    /// stalls holds nothing.
    fn give_up(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        self.inner.release(Some(self.runner), self.room);
        self.file = PathBuf::new();
        self.bytes = 0;
        self.room = 0;
    }

    pub(super) fn into_upload(mut self, entry: PathBuf) -> Upload {
        Upload {
            file: std::mem::take(&mut self.file),
            entry,
            bytes: std::mem::take(&mut self.bytes),
            room: std::mem::take(&mut self.room),
        }
    }
}

impl Drop for Receiving<'_> {
    fn drop(&mut self) {
        if !self.file.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.file);
            self.inner.release(Some(self.runner), self.room);
        }
    }
}

/// Writes the body to the upload's file; false once it passes a quarter of the size limit, the uploads' room runs out
/// or its runner's decision no longer lets it in, which it heeds while the body stalls too. An empty body takes room
/// too, before its file is made.
pub(super) async fn receive(receiving: &mut Receiving<'_>, mut body: Incoming) -> io::Result<bool> {
    let mut decided = receiving.decided.take();
    // After subscribing: a decision made since is seen here or by `decided`.
    if !receiving.wanted() || !receiving.grow(0) {
        receiving.give_up();
        drain(body).await;
        return Ok(false);
    }
    let mut out = tokio::fs::File::create(&receiving.file).await?;
    loop {
        tokio::select! {
            frame = body.frame() => {
                let Some(frame) = frame else {
                    break;
                };
                let frame = frame.map_err(io::Error::other)?;
                if let Ok(data) = frame.into_data() {
                    if !receiving.grow(data.len() as u64) {
                        receiving.give_up();
                        drain(body).await;
                        return Ok(false);
                    }
                    out.write_all(&data).await?;
                }
            }
            Some(Ok(())) = changed(&mut decided) => {
                if !receiving.wanted() {
                    receiving.give_up();
                    drain(body).await;
                    return Ok(false);
                }
            }
        }
    }
    out.flush().await?;
    Ok(true)
}

/// Waits for `decided` to change; `None` at once without one.
async fn changed(
    decided: &mut Option<watch::Receiver<()>>,
) -> Option<Result<(), watch::error::RecvError>> {
    Some(decided.as_mut()?.changed().await)
}

pub(super) async fn drain(mut body: Incoming) {
    while let Some(Ok(_)) = body.frame().await {}
}

/// What an upload gets, stored or not: a tool told its upload failed may stop using the cache for the rest of the
/// build, reads included.
fn accepted() -> Response<Body> {
    status(StatusCode::OK)
}

pub(super) fn status(code: StatusCode) -> Response<Body> {
    let mut response = Response::new(Empty::new().map_err(|never| match never {}).boxed_unsync());
    *response.status_mut() = code;
    response
}
