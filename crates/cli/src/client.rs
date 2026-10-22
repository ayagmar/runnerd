//! The daemon's API over its Unix socket: JSON requests, streamed text and the event stream, as `runnerd_core::api`
//! types. Owns HTTP, timeouts, error decoding and the server-sent events reader, and nothing a terminal shows, so
//! `runnerctl tui` can reuse it. It never talks to Docker or GitHub.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use runnerd_core::api::{ErrorResponse, EventEnvelope, EventQuery, LogQuery};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

/// How long a request may take to answer, and a plain response to arrive in full.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a stream may stay silent. The daemon sends a keep-alive comment on the event stream every 15 s.
const STREAM_IDLE: Duration = Duration::from_secs(45);

/// How long to wait before reconnecting a stream the daemon closed.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// Why a request failed.
#[derive(Debug)]
pub(crate) enum ClientError {
    /// Nothing listens on the socket.
    NotRunning(PathBuf),
    /// The daemon answered with an error.
    Api {
        /// The response's status.
        status: StatusCode,
        /// The daemon's `ErrorResponse.message`, or the body when it sent none.
        message: String,
    },
    /// The daemon did not answer in time.
    Timeout,
    /// The connection or the HTTP exchange failed.
    Transport(String),
    /// The response is not the API type it should be.
    Decode(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotRunning(socket) => write!(
                f,
                "runnerd is not running (nothing listens on {}); start it with `systemctl --user start runnerd`, \
                 or `runnerd serve`",
                socket.display()
            ),
            ClientError::Api { status, message } => write!(f, "{message} ({status})"),
            ClientError::Timeout => write!(
                f,
                "runnerd did not answer within {}s",
                REQUEST_TIMEOUT.as_secs()
            ),
            ClientError::Transport(error) => write!(f, "cannot talk to runnerd: {error}"),
            ClientError::Decode(error) => write!(f, "runnerd sent an unexpected answer: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

fn transport(error: impl fmt::Display) -> ClientError {
    ClientError::Transport(error.to_string())
}

/// A client of the daemon on one socket. Each request opens its own connection.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    socket: PathBuf,
}

impl Client {
    pub(crate) fn new(socket: PathBuf) -> Client {
        Client { socket }
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// `GET path` and decode the JSON answer.
    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let bytes = self.call(Method::GET, path, None).await?;
        serde_json::from_slice(&bytes).map_err(|error| ClientError::Decode(error.to_string()))
    }

    /// `GET path?query` and decode the JSON answer.
    pub(crate) async fn get_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, ClientError> {
        self.get(&with_query(path, query)?).await
    }

    /// `POST path?query` without a body; the answer's body, which may be empty.
    pub(crate) async fn post(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<Bytes, ClientError> {
        self.call(Method::POST, &with_query(path, query)?, None)
            .await
    }

    /// `POST path` with a TOML body, and decode the JSON answer.
    pub(crate) async fn post_toml<T: DeserializeOwned>(
        &self,
        path: &str,
        toml: String,
    ) -> Result<T, ClientError> {
        let body = (toml.into_bytes(), "application/toml");
        let bytes = self.call(Method::POST, path, Some(body)).await?;
        serde_json::from_slice(&bytes).map_err(|error| ClientError::Decode(error.to_string()))
    }

    /// `POST path?query` without a body, and decode the JSON answer.
    pub(crate) async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, ClientError> {
        let bytes = self.post(path, query).await?;
        serde_json::from_slice(&bytes).map_err(|error| ClientError::Decode(error.to_string()))
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<(Vec<u8>, &'static str)>,
    ) -> Result<Bytes, ClientError> {
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            let response = self.send(method, path, body, None).await?;
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(transport)?
                .to_bytes();
            Ok(bytes)
        })
        .await
        .map_err(|_| ClientError::Timeout)?
    }

    /// Sends one request and returns the response once its headers arrived; an error status becomes
    /// [`ClientError::Api`]. The body is read by the caller.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(Vec<u8>, &'static str)>,
        last_event_id: Option<u64>,
    ) -> Result<Response<Incoming>, ClientError> {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                        ClientError::NotRunning(self.socket.clone())
                    }
                    _ => transport(format_args!("{}: {error}", self.socket.display())),
                })?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(transport)?;
        // Drives the connection until the response body is read or dropped.
        tokio::spawn(connection);
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "runnerd");
        let mut bytes = Vec::new();
        if let Some((body, content_type)) = body {
            request = request.header(header::CONTENT_TYPE, content_type);
            bytes = body;
        }
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", HeaderValue::from(id));
        }
        let request = request
            .body(Full::new(Bytes::from(bytes)))
            .map_err(transport)?;
        let response = sender.send_request(request).await.map_err(transport)?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map(http_body_util::Collected::to_bytes)
            .unwrap_or_default();
        Err(ClientError::Api {
            status,
            message: error_message(status, &body),
        })
    }

    /// `GET /v1/jobs/{id}/logs`: the job's log, as lines.
    pub(crate) async fn logs(&self, id: u64, query: &LogQuery) -> Result<Lines, ClientError> {
        let path = with_query(&format!("/v1/jobs/{id}/logs"), query)?;
        let response =
            tokio::time::timeout(REQUEST_TIMEOUT, self.send(Method::GET, &path, None, None))
                .await
                .map_err(|_| ClientError::Timeout)??;
        Ok(Lines::new(response.into_body(), None))
    }

    /// `GET /v1/events`: the event stream, resumed after `last_id` when given.
    pub(crate) async fn events(
        &self,
        query: EventQuery,
        last_id: Option<u64>,
    ) -> Result<Events, ClientError> {
        let mut events = Events {
            client: self.clone(),
            query,
            last_id,
            lines: None,
            closed: 0,
        };
        events.connect().await?;
        Ok(events)
    }
}

/// The daemon's message from an error body, or the body itself, or the status's reason.
fn error_message(status: StatusCode, body: &[u8]) -> String {
    if let Ok(error) = serde_json::from_slice::<ErrorResponse>(body) {
        return error.message;
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        status.canonical_reason().unwrap_or("error").to_owned()
    } else {
        text.to_owned()
    }
}

fn with_query(path: &str, query: &impl Serialize) -> Result<String, ClientError> {
    let query = serde_urlencoded::to_string(query)
        .map_err(|error| ClientError::Decode(error.to_string()))?;
    Ok(if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    })
}

/// A response body read line by line.
#[derive(Debug)]
pub(crate) struct Lines {
    body: Incoming,
    buffer: Vec<u8>,
    done: bool,
    /// How long the body may stay silent; `None` for a followed log, whose job may print nothing for an hour.
    idle: Option<Duration>,
}

impl Lines {
    fn new(body: Incoming, idle: Option<Duration>) -> Lines {
        Lines {
            body,
            buffer: Vec::new(),
            done: false,
            idle,
        }
    }

    /// The next line without its newline; `None` once the body ended. A last line without a newline is returned too.
    /// Fails with [`ClientError::Timeout`] when the body stays silent for longer than its idle time.
    pub(crate) async fn next(&mut self) -> Result<Option<String>, ClientError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            if self.done {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.buffer);
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            let frame = match self.idle {
                Some(idle) => tokio::time::timeout(idle, self.body.frame())
                    .await
                    .map_err(|_| ClientError::Timeout)?,
                None => self.body.frame().await,
            };
            match frame {
                Some(frame) => {
                    if let Ok(data) = frame.map_err(transport)?.into_data() {
                        self.buffer.extend_from_slice(&data);
                    }
                }
                None => self.done = true,
            }
        }
    }
}

/// One server-sent event as read off the wire.
#[derive(Debug, Default, PartialEq, Eq)]
struct RawEvent {
    id: Option<String>,
    data: String,
}

/// Builds server-sent events from lines, as the `EventSource` spec reads them.
#[derive(Debug, Default)]
struct SseParser {
    current: RawEvent,
    has_data: bool,
}

impl SseParser {
    /// Takes one line; returns an event when the line ends one.
    fn line(&mut self, line: &str) -> Option<RawEvent> {
        if line.is_empty() {
            let event = std::mem::take(&mut self.current);
            return std::mem::take(&mut self.has_data).then_some(event);
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if self.has_data {
                    self.current.data.push('\n');
                }
                self.current.data.push_str(value);
                self.has_data = true;
            }
            "id" => self.current.id = Some(value.to_owned()),
            _ => {}
        }
        None
    }
}

/// `GET /v1/events`, read event by event. When the daemon closes the stream (it does for a client too far behind)
/// or the stream stays silent, it reconnects, resuming after the last durable event read. `next` is cancel-safe:
/// dropped while waiting, it loses no event.
#[derive(Debug)]
pub(crate) struct Events {
    client: Client,
    query: EventQuery,
    last_id: Option<u64>,
    lines: Option<(Lines, SseParser)>,
    /// Streams that ended since the last event; a second one in a row is an error.
    closed: u32,
}

impl Events {
    async fn connect(&mut self) -> Result<(), ClientError> {
        let path = with_query("/v1/events", &self.query)?;
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.client.send(Method::GET, &path, None, self.last_id),
        )
        .await
        .map_err(|_| ClientError::Timeout)??;
        self.lines = Some((
            Lines::new(response.into_body(), Some(STREAM_IDLE)),
            SseParser::default(),
        ));
        Ok(())
    }

    /// The next event. Fails when reconnecting failed, as when the daemon stopped, or when the new stream ended
    /// too before sending anything. Events of a type this client does not know (from a newer daemon) are skipped.
    pub(crate) async fn next(&mut self) -> Result<EventEnvelope, ClientError> {
        loop {
            let Some((lines, parser)) = &mut self.lines else {
                if self.closed > 1 {
                    return Err(transport("the daemon keeps closing the event stream"));
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
                self.connect().await?;
                continue;
            };
            let line = match lines.next().await {
                Ok(line) => line,
                Err(ClientError::Timeout) => None,
                Err(error) => return Err(error),
            };
            let Some(line) = line else {
                self.lines = None;
                self.closed += 1;
                continue;
            };
            let Some(raw) = parser.line(&line) else {
                continue;
            };
            if let Some(id) = raw.id.as_deref().and_then(|id| id.parse().ok()) {
                self.last_id = Some(id);
            }
            if let Ok(envelope) = serde_json::from_str::<EventEnvelope>(&raw.data) {
                self.closed = 0;
                return Ok(envelope);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RawEvent, SseParser};

    fn parse(text: &str) -> Vec<RawEvent> {
        let mut parser = SseParser::default();
        text.split('\n')
            .filter_map(|line| parser.line(line))
            .collect()
    }

    #[test]
    fn events_are_split_on_blank_lines_and_keep_alive_comments_are_ignored() {
        let events = parse(
            ":\n\nevent: job.step\nid: 7\ndata: {\"a\":1}\n\n: keep-alive\n\ndata:x\ndata: y\n\n",
        );
        assert_eq!(
            events,
            vec![
                RawEvent {
                    id: Some("7".to_owned()),
                    data: "{\"a\":1}".to_owned()
                },
                RawEvent {
                    id: None,
                    data: "x\ny".to_owned()
                },
            ]
        );
    }
}
