//! The dashboard's client of the daemon's API: HTTP/1 over the daemon's Unix socket, one connection per request.
//! It decodes `runnerd_core::api` types and the event stream, and passes the daemon's errors on as they are. It
//! knows no routes beyond the paths callers give it and keeps no state.

use std::fmt;
use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::http::{Method, Request, StatusCode, header};
use futures_util::{Stream, stream};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use runnerd_core::api::{ErrorResponse, EventEnvelope, EventQuery};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

/// What went wrong talking to the daemon.
#[derive(Debug)]
pub(crate) enum Error {
    /// The socket did not answer.
    Unreachable(String),
    /// The daemon answered with an error.
    Status(StatusCode, String),
    /// The daemon's answer was not what the API says.
    Decode(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unreachable(error) => write!(f, "cannot reach runnerd: {error}"),
            Error::Status(status, message) => write!(f, "runnerd answered {status}: {message}"),
            Error::Decode(error) => write!(f, "unexpected answer from runnerd: {error}"),
        }
    }
}

/// A reply passed on to the browser: the daemon's status and body.
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) body: Bytes,
}

impl Reply {
    /// The daemon's error message, or the body as text.
    pub(crate) fn message(&self) -> String {
        serde_json::from_slice::<ErrorResponse>(&self.body).map_or_else(
            |_| String::from_utf8_lossy(&self.body).into_owned(),
            |error| error.message,
        )
    }
}

/// The daemon at a socket path.
#[derive(Debug, Clone)]
pub(crate) struct Daemon {
    socket: PathBuf,
}

impl Daemon {
    pub(crate) fn new(socket: &Path) -> Daemon {
        Daemon {
            socket: socket.to_owned(),
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<hyper::Response<Incoming>, Error> {
        let unreachable = |error: &dyn fmt::Display| Error::Unreachable(error.to_string());
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|error| unreachable(&format_args!("{}: {error}", self.socket.display())))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| unreachable(&error))?;
        tokio::spawn(async move {
            // Ends with the response body, or with an error the request already returned.
            let _ = connection.await;
        });
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "runnerd");
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|error| Error::Decode(error.to_string()))?;
        sender
            .send_request(request)
            .await
            .map_err(|error| unreachable(&error))
    }

    /// `GET path` (with its query string) as `T`.
    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let body = self.get_bytes(path).await?;
        serde_json::from_slice(&body).map_err(|error| Error::Decode(error.to_string()))
    }

    /// `GET path?query` as `T`.
    pub(crate) async fn get_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, Error> {
        self.get(&with_query(path, query)?).await
    }

    /// `GET path` as text, such as a job's log.
    pub(crate) async fn text(&self, path: &str) -> Result<String, Error> {
        let body = self.get_bytes(path).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn get_bytes(&self, path: &str) -> Result<Bytes, Error> {
        let reply = self.request(Method::GET, path, None).await?;
        if reply.status.is_success() {
            Ok(reply.body)
        } else {
            Err(Error::Status(reply.status, reply.message()))
        }
    }

    /// Sends a request and reads the whole reply, whatever its status.
    pub(crate) async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Reply, Error> {
        let response = self.send(method, path, body).await?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| Error::Unreachable(error.to_string()))?
            .to_bytes();
        Ok(Reply { status, body })
    }

    /// `GET /v1/events`, from now on: every event the query matches, until the daemon ends the stream.
    pub(crate) async fn events(
        &self,
        query: &EventQuery,
    ) -> Result<impl Stream<Item = EventEnvelope> + Send + use<>, Error> {
        let response = self
            .send(Method::GET, &with_query("/v1/events", query)?, None)
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .into_body()
                .collect()
                .await
                .map(http_body_util::Collected::to_bytes);
            let message = body.map_or_else(
                |error| error.to_string(),
                |body| Reply { status, body }.message(),
            );
            return Err(Error::Status(status, message));
        }
        let events = Events {
            body: response.into_body(),
            buffer: Vec::new(),
        };
        Ok(stream::unfold(events, |mut events| async move {
            events.next().await.map(|envelope| (envelope, events))
        }))
    }
}

fn with_query(path: &str, query: &impl Serialize) -> Result<String, Error> {
    let query =
        serde_urlencoded::to_string(query).map_err(|error| Error::Decode(error.to_string()))?;
    Ok(if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    })
}

/// Server-sent events read off a response body.
struct Events {
    body: Incoming,
    buffer: Vec<u8>,
}

impl Events {
    /// The next event; `None` once the stream ends or breaks.
    async fn next(&mut self) -> Option<EventEnvelope> {
        loop {
            while let Some(end) = find_blank_line(&self.buffer) {
                let block: Vec<u8> = self.buffer.drain(..end.0 + end.1).collect();
                if let Some(envelope) = parse_block(&block[..end.0]) {
                    return Some(envelope);
                }
            }
            let frame = self.body.frame().await?.ok()?;
            if let Ok(data) = frame.into_data() {
                self.buffer.extend_from_slice(&data);
            }
        }
    }
}

/// Where the first event block ends: its length and the separator's.
fn find_blank_line(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .map(|at| (at, 2));
    let crlf = buffer
        .windows(4)
        .position(|quad| quad == b"\r\n\r\n")
        .map(|at| (at, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// One SSE block's `data` lines as an event; comments and keep-alives are `None`.
fn parse_block(block: &[u8]) -> Option<EventEnvelope> {
    let text = String::from_utf8_lossy(block);
    let data: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect();
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data.join("\n")).ok()
}
