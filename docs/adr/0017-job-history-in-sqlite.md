# 0017. Job history in SQLite, written by one thread

Amended by [0027](0027-pruning-runs-beside-the-writer.md): pruning runs beside the writer; and by
[0028](0028-job-records-expire.md): job records expire after `history.jobs`.

## Context

Clients draw job timelines, step durations, CPU and memory graphs and trends over weeks, and must see the same
history after the daemon restarts. Clients resume the event stream after a disconnect or a restart.

## Decision

The daemon keeps its history in one SQLite file through `rusqlite` with the bundled SQLite, so a static musl build
needs no system library. One thread owns the write connection and takes writes from a queue (`store::Store`); reads
use a second, read-only connection on Tokio's blocking pool. The schema migrates by `PRAGMA user_version` and the
daemon refuses a database from a newer version. Durable events are rows with increasing IDs, and those IDs are the
SSE event IDs.

`sqlx` was the alternative: async, but its SQLite driver runs a thread per connection anyway, and it brings macros
and a larger dependency tree for queries this small.

## Consequences

Writes never block the engine's tick, and their order is the order the engine made them. History survives restarts
and resuming from `Last-Event-ID` works across them. Samples, logs and events are pruned by the ages in
`[history]`; a job's record and totals are kept.
