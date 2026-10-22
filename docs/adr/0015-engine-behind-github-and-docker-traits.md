# 0015. The engine calls GitHub and Docker through traits

## Context

The engine's recovery, polling order and cleanup rules had no tests: they needed a real Docker and GitHub.

## Decision

`GithubApi` (`crates/core/src/github.rs`) and `DockerApi` (`crates/core/src/docker.rs`) hold the calls the engine and the caches make. `Engine<G, D>` is generic over them, defaulting to the real clients. Tests pass fakes (`crates/core/src/scheduler/engine/tests.rs`).

## Consequences

Engine behaviour is testable without Docker or the network. A new Docker or GitHub call the engine needs goes into the trait and the fakes.
