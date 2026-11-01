# 0073. No compatibility before 0.0.19

Status: **Accepted** (2026-10-26), amended by [0081](0081-waits-leave-out-the-time-no-runnerd-ran.md). Amends [0017](0017-job-history-in-sqlite.md).

## Context

runnerd has no users yet. Its only install runs 0.0.19, with a job history at schema version 15. Code for older
versions had built up anyway:

- fifteen migrations, plus views that let `runnerd doctor` read histories at versions 1 to 13;
- serde defaults for fields an older daemon left out;
- fallbacks in the clients, such as making up a pool for a daemon from before pools, or recomputing a config
  check's budget from the text when a daemon sent none;
- copies of 0.0.7's and 0.0.13's types in tests.

Since 0.0.13, [0061](0061-the-previous-release-reads-the-wire.md)'s check covers the wire between consecutive
releases. Nothing else needs these paths.

## Decision

runnerd keeps no code for anything older than 0.0.19: no migration, default, fallback or test for an older daemon,
client, config, database or image.

- **The history** starts at schema version 15, from `store/schema.sql`, which has the tables, columns and indexes
  the old migrations produced.
  - A new database gets that schema.
  - A version 15 database opens with nothing run.
  - An older one is refused with its version ("move the database aside"), as a newer one already was. Doctor's
    read-only look refuses both the same way.
  - The next schema change adds a migration from version 15.
- **The wire** is guarded from 0.0.19 on by 0061's check. A default that lets 0.0.19 and the current version read
  each other's JSON stays. `RepositoryStatus::profile` and `labels`, the first pool's, were sent until the check
  moved to 0.0.20 for 0.0.19's TUI and dashboard, which coloured a repository by them; then they went, with the
  check accepting that 0.0.20's clients cannot read a newer daemon's status, as nobody runs one beside it.
- **Stored events** from daemons before 0.0.19 that lack a field 0.0.19 always writes no longer parse. A replay
  from before them skips each one with a warning, and `[history] events` prunes them within 30 days. At startup the
  daemon reads only each repository's and profile's latest event. On the one install, every one of those parses.

What stays, because it handles data a version 15 history holds, or a runtime case rather than a version:

- Rows older daemons wrote in a version 15 history: runners without `memory_measure` (peaks counting active page
  cache) or without `admission`, and auto sizing changes without `from_cpus`/`to_cpus`. They are read as before
  until the history prunes them; `auto_sizing` is never pruned.
- Values absent because they are unknown, such as counters a kernel did not report. Also query, request and config
  keys a user may leave out, and GitHub's JSON.
- The clients' best-effort reads: insights, savings and activity leave their figure out when a read fails, rather
  than the whole view.
- The image hides credentials a profile snapshot seeded before 0.0.8 may still hold (`layers.sh`). Snapshots live
  in Docker volumes, and none on the install can be shown to be newer.

## Consequences

A history from before 0.0.19 has to be moved aside before the daemon starts. None exists. Each later release keeps
compatibility with the release before it only, through the wire check. The history's schema changes through
migrations from version 15.
