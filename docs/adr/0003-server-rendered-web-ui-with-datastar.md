# 0003. Web UI: axum, server-rendered HTML and Datastar

Status: **Accepted**.

## Context

The dashboard shows live jobs. A JavaScript build would need TypeScript copies of the API types.

## Decision

The web UI is axum rendering HTML on the server, with Datastar (JS 1.0.x, Rust SDK `datastar`) receiving updates over SSE. No JS build step.

## Consequences

Pages use the core types directly. Interactivity is limited to what Datastar's attributes express. Rejected: SolidJS (needs a build and type copies), htmx (polls, or needs an extension for SSE).
