# 0016. Strict lints, every exception with a reason

Status: **Accepted**.

## Context

Agents and new contributors write most changes. Review should not be the first place `unwrap`, stray prints or undocumented public items are caught.

## Decision

`[workspace.lints]` in `Cargo.toml` turns on clippy's pedantic group and a set of restriction lints; CI denies warnings. A lint that fights the code's style is allowed in `Cargo.toml` with a comment. Any `#[allow]` in code needs `reason = "..."`. `runnerd-core` requires docs on public items. Tests may unwrap and panic (`clippy.toml`).

## Consequences

A new lint is kept only if it needs a handful of allows at most.
