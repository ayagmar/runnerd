# 0002. JSON and SSE over a Unix socket; the web UI on loopback TCP

Status: **Accepted**.

## Context

The CLI and the desktop app run as the same user on the same machine. A browser can only speak HTTP over TCP.

## Decision

The daemon serves JSON on a Unix socket, `$XDG_RUNTIME_DIR/runnerd.sock`, with mode 0600, plus SSE for live updates ([0017](0017-job-history-in-sqlite.md)). The web dashboard is served on a loopback TCP port.

## Consequences

The CLI and desktop app need no network listener, and the socket's peer credentials authenticate them (`crates/daemon/src/api.rs`). The web port needs its own authentication: a one-time login and a session cookie ([0020](0020-dashboard-is-an-api-client-on-loopback.md)).
