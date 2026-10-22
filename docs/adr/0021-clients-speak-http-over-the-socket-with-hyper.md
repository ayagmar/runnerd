# 0021. Clients speak HTTP over the socket with hyper

## Context

Clients may not build `reqwest` (ADR 0014, `check-boundaries.sh`): it brings TLS and a connection pool a client of
a local Unix socket does not need, and is how the daemon reaches GitHub. `runnerctl` still needs HTTP/1.1 with
chunked bodies for followed logs and the event stream.

## Decision

`runnerctl` uses `hyper`'s HTTP/1 client connection over a Tokio `UnixStream`, one connection per request, with
`http-body-util` to read bodies. The socket client and the server-sent events reader are one module,
`crates/cli/src/client.rs`, which `runnerctl tui` reuses. Query strings are the API's query types encoded with
`serde_urlencoded`.

## Consequences

No new crate in the lockfile: hyper, hyper-util and http-body-util already come with axum and reqwest. Timeouts,
reconnecting the event stream and decoding `ErrorResponse` are the client module's own code.
