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

use super::{Inner, UPLOADS, Upload, Writes, valid_segment};
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
    let known = lock(&inner.state).entries.get(&entry).copied();
    let path = inner.root.join(&entry);
    let file = match known {
        Some(_) => tokio::fs::File::open(&path).await.ok(),
        None => None,
    };
    let (Some(known), Some(file)) = (known, file) else {
        if counted && let Some(open) = lock(&inner.state).runners.get_mut(runner) {
            open.counts.misses += 1;
        }
        return status(StatusCode::NOT_FOUND);
    };
    if counted {
        let mut state = lock(&inner.state);
        let now = crate::time::now_ms();
        if let Some(used) = state.entries.get_mut(&entry) {
            used.used = now;
        }
        if let Some(open) = state.runners.get_mut(runner) {
            open.counts.hits += 1;
            open.counts.read_bytes += known.bytes;
        }
        // Its time survives a restart, which counts entries by when they were last modified.
        tokio::task::spawn_blocking(move || {
            if let Ok(file) = std::fs::File::open(&path) {
                let _ = file.set_modified(std::time::SystemTime::now());
            }
        });
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
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, known.bytes.into());
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
            inner.root.join(UPLOADS).join(state.next_upload.to_string()),
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
    let bytes = match receive(body, &upload, max_entry).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            let _ = tokio::fs::remove_file(&upload).await;
            return refused();
        }
        Err(error) => {
            debug!("{runner}: a build cache upload failed: {error}");
            let _ = tokio::fs::remove_file(&upload).await;
            return status(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let upload = Upload {
        file: upload,
        entry,
        bytes,
    };
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
        let _ = tokio::fs::remove_file(&upload.file).await;
        refused()
    }
}

/// Writes the body to `file`; `None` once it passes `max` bytes.
async fn receive(mut body: Incoming, file: &std::path::Path, max: u64) -> io::Result<Option<u64>> {
    let mut out = tokio::fs::File::create(file).await?;
    let mut bytes = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(io::Error::other)?;
        if let Ok(data) = frame.into_data() {
            bytes += data.len() as u64;
            if bytes > max {
                drop(out);
                drain(body).await;
                return Ok(None);
            }
            out.write_all(&data).await?;
        }
    }
    out.flush().await?;
    Ok(Some(bytes))
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
