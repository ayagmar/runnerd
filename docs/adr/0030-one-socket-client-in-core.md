# 0030. One socket client for every client, in core

Supersedes the part of [0021](0021-clients-speak-http-over-the-socket-with-hyper.md) that put the client in
`crates/cli`.

## Context

The dashboard (`crates/web`) and `runnerctl` each had their own HTTP client over the socket and their own
server-sent events reader. The two readers split events differently, only `runnerctl`'s reconnected with
`Last-Event-ID`, and the web crate pinned its own versions of hyper and its helpers. A fix to one did not reach the
other, and the desktop app would have made a third.

## Decision

The client is `runnerd_core::client`, behind a `client` feature of `runnerd-core` that `host` does not need and
clients turn on. It is `runnerctl`'s client moved as it was: one hyper HTTP/1 connection per request, timeouts,
`ErrorResponse` decoding, line-based event parsing, and an event reader that reconnects and resumes after the last
durable event. It adds `Client::request`, which returns an error status as a reply, for the dashboard's pass-through
of refusals. A feature rather than a new crate, because the crates are published together and a new one would need
its own first publish and trusted publisher before a release could go out.

## Consequences

`runnerctl`, its terminal UI and the dashboard read events the same way; a dashboard page's stream now survives the
daemon closing the event stream, and ends (so the page reconnects and reads afresh) only when nothing could be resumed.
hyper, hyper-util, http-body-util and serde_urlencoded are workspace dependencies used through core, and the
boundary check is unchanged: `client` brings no Docker, GitHub or TLS code.
