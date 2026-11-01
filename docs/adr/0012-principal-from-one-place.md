# 0012. Handlers take a principal from one place

Status: **Accepted**.

## Context

Remote hosts, later, add SSH and Tailscale transports. Each must not need its own copy of the API.

## Decision

Transports only produce an `api::Principal`. Handlers get it from one extractor (`Authenticated` in `crates/daemon/src/api.rs`) and never look at the transport.

## Consequences

A new transport is a listener plus the code that maps its identity to a principal.
