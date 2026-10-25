# 0062. Client parity is checked, not remembered

Status: **Accepted** (2026-10-25).

## Context

[ADR 0059](0059-clients-show-what-core-computes.md) and [ADR 0060](0060-clients-show-what-core-computes-about-the-host.md)
moved what the dashboard, the TUI and `runnerctl` show of a job and of the host into core, after an audit found 30
places where they computed or worded the same fact apart, 12 of them contradicting each other. Nothing kept the next
helper from going back into a client. Each client's tests used fixtures of their own, so two clients could disagree
and both pass.

## Decision

- `crates/fixtures` (`runnerd-fixtures`, not published) holds one set of daemon data chosen where clients drifted
  before: lost, elsewhere, `startup_failure`, timed-out and cancelled jobs, an unnamed adopted job, a step without an
  end time and a cancelled one with times, a job no pool serves, a held measured budget, warm and starting runners,
  a slot whose job is known before GitHub lists its runner busy, caps past the budget, records near `max_total`, and
  a build cache that only stored and refused. Its `facts` computes from them, through core alone, what a client must
  say.
- Each client renders those fixtures through its own path and asserts the facts it shows: the dashboard's pages from
  its fake daemon, the TUI's `TestBackend` frames, `runnerctl`'s text. They assert facts and the class, style or
  colour of their meaning, not layout. A client depends on the fixtures as a dev-dependency only, so none depends on
  another.
- `.github/scripts/check-boundaries.sh` fails when the cli, the dashboard or the daemon defines a function named for
  a formatter core owns (`duration`, `ago`, `bytes`, `cpus`, `cpu_time` and the like) that returns text, or for a
  classifier core owns (`outcome`, `is_failure`, `title`, `waited`, `ran`, `slot_state` and the like) that takes the
  API type it decides from, or when a client reads a runner's `busy` or `warm`. Each exception is listed in the
  script with why. Mapping core's kind or tone to a colour stays a client's.

## Consequences

- A fact a client words or decides itself fails a test or the boundary check, not a reader of two screens. The
  first run found the TUI cutting `startup failure` to `STARTUP F` in its outcome column.
- A new tricky case is one fixture and one fact in `crates/fixtures`, which every client's parity test then asserts
  as far as it shows it.
- The dashboard reads the wall clock, so its parity test checks only facts that do not change with it.
- The name check is a tripwire, not a proof: a helper named otherwise passes it. The parity tests catch what it
  misses for the facts the fixtures hold.
