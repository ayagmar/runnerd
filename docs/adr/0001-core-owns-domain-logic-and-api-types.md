# 0001. `runnerd-core` owns domain logic and API types

## Context

The daemon, the CLI, the web dashboard and the desktop app all need the same request and response types and the same rules about config and budgets.

## Decision

Domain logic and API types live only in `runnerd-core` (`crates/core`). Other crates call it; none redefines a type it exports.

## Consequences

Every client deserializes the daemon's own types, so an API change is one edit plus the compiler's list of call sites. Core grows large; [0014](0014-host-feature-for-daemon-only-code.md) keeps clients from building all of it.
