//! A client of the daemon's API over its Unix socket, for every client crate: JSON requests, streamed text and the
//! event stream, as [`api`](crate::api) types. Owns HTTP, timeouts, error decoding and the server-sent events reader,
//! with reconnecting and `Last-Event-ID`; it knows no routes beyond the paths callers give it and nothing about how a
//! client shows an answer. It never talks to Docker or GitHub. Built with the `client` feature.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

pub use hyper::{Method, StatusCode};

use crate::api::{Busy, ErrorResponse, EventEnvelope, EventQuery, LogQuery, RunningJob};

/// How long the client waits for the daemon.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// For a request to be answered, and a plain response to arrive in full; for a log that is not followed, for
    /// each part of its body. A forced pause cancels runs on GitHub and removes containers before it answers.
    pub request: Duration,
    /// For the event stream to send something. The daemon sends a keep-alive comment every 15 s.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Timeouts {
        Timeouts {
            request: Duration::from_secs(30),
            idle: Duration::from_secs(45),
        }
    }
}

/// How long to wait before reconnecting a stream the daemon closed.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// Why a request failed.
#[derive(Debug)]
pub enum ClientError {
    /// Nothing listens on the socket.
    NotRunning(PathBuf),
    /// The daemon answered with an error.
    Api {
        /// The response's status.
        status: StatusCode,
        /// The daemon's `ErrorResponse.message`, or the body when it sent none.
        message: String,
        /// The runners in the way, when the daemon refused with a [`Busy`]; else empty.
        jobs: Vec<RunningJob>,
    },
    /// The daemon did not answer within this long.
    Timeout(Duration),
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
            ClientError::Api {
                status, message, ..
            } => write!(f, "{message} ({status})"),
            ClientError::Timeout(after) => {
                write!(f, "runnerd did not answer within {}s", after.as_secs())
            }
            ClientError::Transport(error) => write!(f, "cannot talk to runnerd: {error}"),
            ClientError::Decode(error) => write!(f, "runnerd sent an unexpected answer: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

fn transport(error: impl fmt::Display) -> ClientError {
    ClientError::Transport(error.to_string())
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ClientError> {
    serde_json::from_slice(bytes).map_err(|error| ClientError::Decode(error.to_string()))
}

/// A request body and its media type.
#[derive(Debug)]
pub struct Body {
    content_type: &'static str,
    bytes: Vec<u8>,
}

impl Body {
    /// A config file, as `PUT /v1/config` and `POST /v1/config/check` take it.
    pub fn toml(text: String) -> Body {
        Body {
            content_type: "application/toml",
            bytes: text.into_bytes(),
        }
    }
}

/// The daemon's whole answer, whatever its status, for a client that passes it on.
#[derive(Debug)]
pub struct Reply {
    /// The response's status.
    pub status: StatusCode,
    /// Its body.
    pub body: Bytes,
}

impl Reply {
    /// The daemon's `ErrorResponse.message`, or the body itself, or the status's reason.
    pub fn message(&self) -> String {
        error_message(self.status, &self.body)
    }
}

/// A client of the daemon on one socket. Each request opens its own connection.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    timeouts: Timeouts,
}

impl Client {
    /// A client of the daemon listening on `socket`, with the default timeouts.
    pub fn new(socket: PathBuf) -> Client {
        Client {
            socket,
            timeouts: Timeouts::default(),
        }
    }

    /// The same client, waiting this long instead.
    #[must_use]
    pub fn with_timeouts(self, timeouts: Timeouts) -> Client {
        Client { timeouts, ..self }
    }

    /// Waits for `future` at most the request timeout.
    async fn in_time<T>(
        &self,
        future: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        let after = self.timeouts.request;
        tokio::time::timeout(after, future)
            .await
            .map_err(|_| ClientError::Timeout(after))?
    }

    /// The socket it connects to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// `GET path` and decode the JSON answer.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        decode(&self.call(Method::GET, path, None).await?)
    }

    /// `GET path?query` and decode the JSON answer.
    pub async fn get_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, ClientError> {
        self.get(&with_query(path, query)?).await
    }

    /// `GET path` as text, such as a job's whole log.
    pub async fn text(&self, path: &str) -> Result<String, ClientError> {
        let bytes = self.call(Method::GET, path, None).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `POST path?query` without a body; the answer's body, which may be empty.
    pub async fn post(&self, path: &str, query: &impl Serialize) -> Result<Bytes, ClientError> {
        self.call(Method::POST, &with_query(path, query)?, None)
            .await
    }

    /// `POST path` with a TOML body, and decode the JSON answer.
    pub async fn post_toml<T: DeserializeOwned>(
        &self,
        path: &str,
        toml: String,
    ) -> Result<T, ClientError> {
        decode(
            &self
                .call(Method::POST, path, Some(Body::toml(toml)))
                .await?,
        )
    }

    /// `POST path?query` without a body, and decode the JSON answer.
    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, ClientError> {
        decode(&self.post(path, query).await?)
    }

    /// Sends a request and reads the whole answer; an error status is a [`Reply`] too, not a [`ClientError`].
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Body>,
    ) -> Result<Reply, ClientError> {
        self.in_time(async {
            let response = self.exchange(method, path, body, None).await?;
            let status = response.status();
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(transport)?
                .to_bytes();
            Ok(Reply { status, body })
        })
        .await
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Body>,
    ) -> Result<Bytes, ClientError> {
        self.in_time(async {
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
    }

    /// Sends one request and returns the response once its headers arrived; an error status becomes
    /// [`ClientError::Api`]. The body is read by the caller.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Body>,
        last_event_id: Option<u64>,
    ) -> Result<Response<Incoming>, ClientError> {
        let response = self.exchange(method, path, body, last_event_id).await?;
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
        let jobs = serde_json::from_slice::<Busy>(&body)
            .map(|busy| busy.jobs)
            .unwrap_or_default();
        Err(ClientError::Api {
            status,
            message: error_message(status, &body),
            jobs,
        })
    }

    /// Sends one request and returns the response once its headers arrived, whatever its status.
    async fn exchange(
        &self,
        method: Method,
        path: &str,
        body: Option<Body>,
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
        if let Some(body) = body {
            request = request.header(header::CONTENT_TYPE, body.content_type);
            bytes = body.bytes;
        }
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", HeaderValue::from(id));
        }
        let request = request
            .body(Full::new(Bytes::from(bytes)))
            .map_err(transport)?;
        sender.send_request(request).await.map_err(transport)
    }

    /// `GET /v1/jobs/{id}/logs`: the job's log, as lines. Unless followed, each part of the body must come within the
    /// request timeout.
    pub async fn logs(&self, id: u64, query: &LogQuery) -> Result<Lines, ClientError> {
        let path = with_query(&format!("/v1/jobs/{id}/logs"), query)?;
        let response = self
            .in_time(self.send(Method::GET, &path, None, None))
            .await?;
        // A followed log may stay quiet as long as its job does; a finished one comes at once.
        let idle = (!query.follow).then_some(self.timeouts.request);
        Ok(Lines::new(response.into_body(), idle))
    }

    /// `GET /v1/events`: the event stream, resumed after `last_id` when given.
    pub async fn events(
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
            missed: false,
            reopened: false,
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
pub struct Lines {
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
    pub async fn next(&mut self) -> Result<Option<String>, ClientError> {
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
                    .map_err(|_| ClientError::Timeout(idle))?,
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

/// `GET /v1/events`, read event by event. When the daemon closes the stream (it does for a client too far behind),
/// the connection breaks or the stream stays silent, it reconnects, resuming after the last durable event read.
/// Dropping a pending `next` loses no event but restarts the idle timeout, so a caller that also waits on something
/// else (a redraw tick) keeps one `next` pending across it, as `runnerctl watch` does. With no durable event read yet
/// there is nothing to resume after, so the new stream is live only and what happened in between is lost:
/// [`Events::missed_events`] says so, and a caller that keeps state reads it again. Live events (samples, log lines)
/// carry no ID and are never replayed: [`Events::reopened`] says when the stream was opened again at all.
#[derive(Debug)]
pub struct Events {
    client: Client,
    query: EventQuery,
    last_id: Option<u64>,
    lines: Option<(Lines, SseParser)>,
    /// Streams that ended since the last event; a second one in a row is an error.
    closed: u32,
    /// Set when a stream was opened again with no event to resume after.
    missed: bool,
    /// Set when a stream was opened again, resumed or not.
    reopened: bool,
}

impl Events {
    /// Whether, since the last call, the stream was opened again with no event ID to resume after, so events from
    /// the time it was closed are lost. Cleared by the call.
    pub fn missed_events(&mut self) -> bool {
        std::mem::take(&mut self.missed)
    }

    /// Whether, since the last call, the stream was opened again, so live events from the time it was closed are
    /// lost even when it resumed after the last durable one. Cleared by the call.
    pub fn reopened(&mut self) -> bool {
        std::mem::take(&mut self.reopened)
    }

    async fn connect(&mut self) -> Result<(), ClientError> {
        let path = with_query("/v1/events", &self.query)?;
        let response = self
            .client
            .in_time(self.client.send(Method::GET, &path, None, self.last_id))
            .await?;
        self.lines = Some((
            Lines::new(response.into_body(), Some(self.client.timeouts.idle)),
            SseParser::default(),
        ));
        Ok(())
    }

    /// The next event. Fails when reconnecting failed, as when the daemon stopped, or when the new stream ended
    /// too before sending anything. Events of a type this client does not know (from a newer daemon) are skipped.
    pub async fn next(&mut self) -> Result<EventEnvelope, ClientError> {
        loop {
            let Some((lines, parser)) = &mut self.lines else {
                if self.closed > 1 {
                    return Err(transport("the daemon keeps closing the event stream"));
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
                self.connect().await?;
                self.missed |= self.last_id.is_none();
                self.reopened = true;
                continue;
            };
            // A silent stream, and one cut off mid-body as when the daemon is killed, end like a closed one.
            let line = match lines.next().await {
                Ok(line) => line,
                Err(ClientError::Timeout(_) | ClientError::Transport(_)) => None,
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests: a fake daemon that cannot start fails the test"
)]
mod tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::routing::get;
    use futures_util::{StreamExt, stream};
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::HeaderMap;

    use super::{Client, ClientError, RawEvent, SseParser, Timeouts};
    use crate::api::{Budget, Event, EventEnvelope, EventQuery, LogQuery};
    use crate::testdir::TestDir;

    /// A fake daemon on a temporary socket serving `router` until the test ends, and a client of it whose requests
    /// time out after `request` and whose streams after `idle` of silence. Never the user's socket.
    struct Fake {
        _dir: TestDir,
        client: Client,
    }

    impl Fake {
        fn serve(router: Router, request: Duration, idle: Duration) -> Fake {
            let dir = TestDir::new("client");
            let socket = dir.0.join("runnerd.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Fake {
                _dir: dir,
                client: Client::new(socket).with_timeouts(Timeouts { request, idle }),
            }
        }
    }

    /// A 200 response whose body never comes.
    fn stalled() -> Body {
        Body::from_stream(stream::pending::<Result<Bytes, Infallible>>())
    }

    fn budget_event(id: u64) -> Bytes {
        let envelope = EventEnvelope {
            id: Some(id),
            at: 1_700_000_000,
            event: Event::BudgetChanged {
                budget: Budget {
                    slice: "runnerd.slice".to_owned(),
                    memory_max: None,
                    memory_in_use: id,
                    mirror_memory: 0,
                    seed_memory: Vec::new(),
                },
            },
        };
        Bytes::from(format!(
            "event: budget.changed\nid: {id}\ndata: {}\n\n",
            serde_json::to_string(&envelope).unwrap()
        ))
    }

    /// The first stream sends event 7 and breaks off mid-body, as when the daemon is killed; the next sends 8.
    async fn breaking_stream(
        State(seen): State<Arc<Mutex<Vec<Option<String>>>>>,
        headers: HeaderMap,
    ) -> Body {
        let mut seen = seen.lock().unwrap();
        seen.push(
            headers
                .get("last-event-id")
                .map(|id| id.to_str().unwrap().to_owned()),
        );
        if seen.len() == 1 {
            // The break comes after event 7 reached the client.
            Body::from_stream(
                stream::iter([Ok(budget_event(7))]).chain(stream::once(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Err(std::io::Error::other("killed"))
                })),
            )
        } else {
            Body::from_stream(
                stream::iter([Ok::<_, std::io::Error>(budget_event(8))]).chain(stream::pending()),
            )
        }
    }

    #[tokio::test]
    async fn an_event_stream_cut_off_mid_body_resumes_after_the_last_event() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake::serve(
            Router::new()
                .route("/v1/events", get(breaking_stream))
                .with_state(seen.clone()),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        let client = &fake.client;
        let read = async {
            let mut events = client.events(EventQuery::default(), None).await?;
            let first = events.next().await?.id;
            let reopened_before = events.reopened();
            let second = events.next().await?.id;
            Ok::<_, ClientError>((first, second, reopened_before, events.reopened()))
        };
        let read = tokio::time::timeout(Duration::from_secs(10), read)
            .await
            .expect("the events should come");
        assert_eq!(
            read.unwrap(),
            (Some(7), Some(8), false, true),
            "resumed after 7, and says it was opened again"
        );
        assert_eq!(*seen.lock().unwrap(), [None, Some("7".to_owned())]);
    }

    #[tokio::test]
    async fn a_log_that_is_not_followed_fails_when_its_body_stalls() {
        let fake = Fake::serve(
            Router::new().route("/v1/jobs/{id}/logs", get(|| async { stalled() })),
            Duration::from_millis(300),
            Duration::from_secs(60),
        );
        let client = &fake.client;
        let read = async {
            let mut lines = client.logs(1, &LogQuery::default()).await?;
            lines.next().await
        };
        let read = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .expect("the read should fail, not hang");
        assert!(matches!(read, Err(ClientError::Timeout(_))), "{read:?}");
    }

    #[tokio::test]
    async fn events_with_crlf_line_ends_are_read() {
        let crlf = || async {
            let event = String::from_utf8(budget_event(3).to_vec()).unwrap();
            let body = Bytes::from(event.replace('\n', "\r\n"));
            Body::from_stream(stream::iter([Ok::<_, Infallible>(body)]).chain(stream::pending()))
        };
        let fake = Fake::serve(
            Router::new().route("/v1/events", get(crlf)),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        let read = async {
            let mut events = fake.client.events(EventQuery::default(), None).await?;
            events.next().await
        };
        let event = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .expect("the event should come");
        assert_eq!(event.unwrap().id, Some(3));
    }

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
