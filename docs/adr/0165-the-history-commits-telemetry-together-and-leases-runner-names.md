# 0165. The history commits telemetry together and leases runner names

Status: **Accepted** (2026-11-24). Amends [0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md): after a
restart, runner names run ahead of the clock by up to five minutes. No wire or schema change.

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
  gathers those sent within 1 s of the first, up to 256 writes or half the log backlog, into one transaction, each
  write in a savepoint: one that fails rolls back and is logged, the others are kept. Any other write ends the wait
  at once and is applied next, so the order writes were sent in holds, and a flush, a runner's name or a durable change
  never waits for the window. Events go out after the commit, in order; a host sample's live event goes out whether or
  not the batch committed. A log batch's share of the backlog is given back once its transaction is done.
- **Counters are written when they rise.** A runner's `ooms`, `local_ooms` and `sock_throttled` are written only when
  unknown (`NULL`) or lower than the sample's count. The first sample of a kernel that counts writes 0 once, so
  `hit_cap` and `own_cap` still read `Some(false)` rather than unknown.
- **Runner names are leased.** A name whose second is before the end of the writer's lease needs no write. One at or
  past it stores a new end, `secs + 300`, as `meta.runner_named`, synced to disk with `synchronous = FULL`, and the
  stored value never goes back. Each daemon takes its own lease with its first name. A restart reads
  `meta.runner_named` as the latest second named, as before, so it names past the whole lease. The second in a name
  only tells names apart, so a name ahead of the clock means nothing else.
- **Multi-row writes are one transaction each**: waits for the host and for a slot, re-runs, cache sizes.
- **SQLite settings**: `wal_autocheckpoint = 8192` (32 MiB), `journal_size_limit` 64 MiB, `temp_store = MEMORY`.

## Consequences

- The same replay writes 25 MB in two minutes instead of 85 MB. A 2 s window would write 19 MB; 1 s keeps the
  live log within a second.
- Live log lines and samples reach clients up to 1 s later. A crash loses up to 1 s of telemetry, its log position
  with it, so a capture after a restart reads those lines again.
- The WAL grows to 32 MiB between checkpoints, and a burst leaves it at 64 MiB at most.
- 0.1.5 reads `meta.runner_named` with the same query and names past the lease; when it writes the second it named,
  that is past the lease, and this build leases past it. Nothing else is stored differently.
