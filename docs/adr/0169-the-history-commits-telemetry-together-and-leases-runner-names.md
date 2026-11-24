# 0169. The history commits telemetry together and leases runner names

Status: **Accepted** (2026-11-24). Amends [0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md): after a
restart, runner names run ahead of the clock, by up to five minutes per restart that starts one. No wire or schema change.

## Context

A busy host's daemon wrote about 69 GB a day to `runnerd.db`'s WAL while keeping about 0.3 GB of new data. A replay of
four noisy runners through `Store` wrote 85 MB in two minutes, about 61 GB a day. Four causes:

- Every runner start switched SQLite to `synchronous = FULL`, wrote `meta.runner_named` and synced it.
- Every 2 s sample wrote `runners.ooms`, `local_ooms` and `sock_throttled` for each runner, unchanged or not.
- Each log batch (every 200 ms per noisy runner) and each sampler pass committed alone, so each rewrote whole pages of
  `jobs`, `log_lines`, the log outline, `samples` and the indices.
- Waits, re-runs and cache sizes committed once per row. SQLite checkpointed the WAL every 1000 pages (4 MiB), copying
  a page the writer changes again and again into the database each time.

## Decision

- **Telemetry commits together.** Log lines, runner samples and host samples are `store::Telemetry`. The writer
  gathers those sent within 1 s of the first into one batch, up to 256 writes and, unless its first write alone holds
  more, 4 MiB of log (half the backlog). These are batching targets, not latency bounds: a capture that waits for a
  larger share of the backlog than is free may wait out the window. Any other write ends the batch at once and is
  applied next, so writes keep the order they were sent in, and a flush, a runner's name or a durable change never
  waits for the window.
- **Each stored write has its own savepoint.** A write that fails rolls back alone and is logged; the others are kept.
  When SQLite rolls the whole transaction back itself, as on a full disk, nothing of it was kept, so each write is
  tried once more in a transaction of its own, in order. When the transaction never began, or its commit failed, which
  may have kept it, nothing is tried again. Events go out after the commit, in the order the writes were sent, only for
  writes kept.
- **Host samples do not wait on the database.** Their live events are taken out of the batch before any SQL and go
  out in order with the rest, whatever becomes of the stored writes; a batch of host samples alone opens no
  transaction. Storing the held ones, a minute's worth at a time, is its own transaction after.
- **A capture's log is a chain.** Each capture's lines carry where they start in the runner's log (`LogChain`). The
  writer stores them only if the job's stored position is that start, and no earlier line of the chain was refused;
  otherwise it refuses them, storing and announcing nothing, and refuses the rest of the chain. The capture then
  breaks off, and `recapture` starts it again from the stored position, so a segment that failed is captured again
  rather than skipped. Lines a capture saw before its job are dropped, as before, and its next lines are not checked. Lines of a job
  the history has no row for are dropped, as before, without breaking the chain; once the row comes, the next lines
  do not start where its empty log does, so the capture starts again from the start.
- **Counters are written when they rise.** A runner's `ooms`, `local_ooms` and `sock_throttled` are written only when
  unknown (`NULL`) or lower than the sample's count. The first sample of a kernel that counts writes 0 once, so
  `hit_cap` and `own_cap` still read `Some(false)` rather than unknown.
- **The writer grants runner names.** The engine proposes a second; the writer grants it, or a later one: on its first
  grant it reads the latest second the history knows was named (`read::last_runner_second`: the runners' and jobs'
  names and `meta.runner_named`), and every grant is past that and past the last one it granted. So a name is never
  given twice, even when the engine's own read at its start failed or the clock stepped back. A grant before the end
  of the writer's lease needs no write. One at or past it stores a new end, the grant plus 300 s, as
  `meta.runner_named`, synced to disk with `synchronous = FULL`; the stored value never goes back. A restart reads it
  as named, so it names past the whole lease.
- **Multi-row writes are one transaction each**: waits for the host and for a slot, re-runs, cache sizes.
- **SQLite settings**: `wal_autocheckpoint = 8192` pages (32 MiB), `journal_size_limit` 64 MiB, `temp_store =
  MEMORY`.

## Consequences

- The same replay writes 25 MB in two minutes instead of 85 MB. A 2 s window would write 19 MB; 1 s keeps the live
  log and samples within about a second.
- A process crash loses the batch being gathered, up to about a second of telemetry, with its log position, so a
  capture after a restart reads those lines again. A power loss can also take back telemetry committed since the last
  sync, as `synchronous = NORMAL` always could; the name lease is synced and survives it.
- Each lease reaches 300 s past the second it was taken for. A restart that starts a runner names past the last
  lease and takes a new one, so after several quick restarts names can run further ahead of the clock than that. The
  second in a name only tells names apart.
- The WAL is checkpointed every 32 MiB, and cut back to 64 MiB after a checkpoint. Neither is a cap: a reader that
  holds an old snapshot keeps the WAL from being reused, as before.
- `meta.runner_named` stays a second at or past every one named, which any runnerd reads with the same query. That
  is all this keeps compatible: a 0.1.5 daemon still refuses this history in place, which is at schema 32, past the
  28 it reads.
