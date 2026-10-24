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
use tracing::debug;

use super::{Inner, Upload, Writes, valid_segment};
use crate::sync::lock;

type Body = UnsyncBoxBody<Bytes, io::Error>;

/// The tools whose requests it answers, each under its own path: `/<tool>/<key>`. Each reads with `GET`, a 404
/// being a miss, and writes with `PUT`; Bazel's keys are `ac/<sha256>` and `cas/<sha256>`, sccache's
/// `<a>/<b>/<c>/<key>`.
const TOOLS: [&str; 3] = ["gradle", "sccache", "bazel"];

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

async fn handle(inner: &Inner, runner: &str, request: Request<Incoming>) -> Response<Body> {
    let Some(repository) = lock(&inner.state)
        .runners
        .get(runner)
        .map(|open| open.repository.clone())
    else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let path = request.uri().path().trim_start_matches('/').to_owned();
    let Some((tool, rest)) = path.split_once('/') else {
        return status(StatusCode::NOT_FOUND);
    };
    if !TOOLS.contains(&tool) {
        return status(StatusCode::NOT_FOUND);
    }
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

    let (writes, max_entry, upload) = {
        let mut state = lock(&inner.state);
        let writes = state.runners.get(runner).map(|open| open.writes);
        state.next_upload += 1;
        (
            writes,
            state.max_size / 4,
            inner.uploads.join(state.next_upload.to_string()),
        )
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
    let mut receiving = Receiving {
        inner,
        runner,
        file: upload,
        bytes: 0,
    };
    match receive(&mut receiving, body, max_entry).await {
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
            Some(open) if open.writes == Writes::Undecided => {
                open.held.push(upload);
                return accepted();
            }
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

/// An upload being received. Unless it becomes an `Upload`, its file goes and the uploads' room it took is given
/// back, also when its connection is cut midway, as when its runner is removed.
struct Receiving<'inner> {
    inner: &'inner Inner,
    runner: &'inner str,
    file: PathBuf,
    bytes: u64,
}

impl Receiving<'_> {
    fn into_upload(mut self, entry: PathBuf) -> Upload {
        let bytes = std::mem::take(&mut self.bytes);
        Upload {
            file: std::mem::take(&mut self.file),
            entry,
            bytes,
        }
    }
}

impl Drop for Receiving<'_> {
    fn drop(&mut self) {
        if !self.file.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.file);
            self.inner.release(Some(self.runner), self.bytes);
        }
    }
}

/// Writes the body to the upload's file; false once it passes `max` bytes or the uploads' room runs out.
async fn receive(receiving: &mut Receiving<'_>, mut body: Incoming, max: u64) -> io::Result<bool> {
    let mut out = tokio::fs::File::create(&receiving.file).await?;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(io::Error::other)?;
        if let Ok(data) = frame.into_data() {
            let length = data.len() as u64;
            if receiving.bytes + length > max || !receiving.inner.reserve(receiving.runner, length)
            {
                drop(out);
                drain(body).await;
                return Ok(false);
            }
            receiving.bytes += length;
            out.write_all(&data).await?;
        }
    }
    out.flush().await?;
    Ok(true)
}

async fn drain(mut body: Incoming) {
    while let Some(Ok(_)) = body.frame().await {}
}

/// What an upload gets, stored or not: a tool told its upload failed may stop using the cache for the rest of the
/// build, reads included.
fn accepted() -> Response<Body> {
    status(StatusCode::OK)
}

fn status(code: StatusCode) -> Response<Body> {
    let mut response = Response::new(Empty::new().map_err(|never| match never {}).boxed_unsync());
    *response.status_mut() = code;
    response
}
