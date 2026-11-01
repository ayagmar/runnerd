# 0014. Daemon-only code behind core's `host` feature

Status: **Accepted**.

## Context

Clients need `api` and `config` from `runnerd-core`, but the crate also holds the Docker client (`bollard`), the GitHub client (`reqwest`) and the scheduler. Documentation alone did not stop a client from depending on them.

## Decision

`runnerd-core` has a `host` feature, on by default, that gates `docker`, `github`, `cache`, `isolation`, `observe`, `scheduler` and `store`. The workspace dependency turns default features off; only the daemon turns `host` on. `.github/scripts/check-boundaries.sh` fails CI when a client builds `bollard`, `reqwest` or `host`.

## Consequences

Clients build fast and cannot reach Docker or GitHub. A type that clients need must live in `api` or `config`, not in a `host` module.
