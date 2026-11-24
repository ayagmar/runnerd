# 0171. The history packs final logs and reads events by type

Status: **Accepted** (2026-11-24). Amends [0017](0017-job-history-in-sqlite.md) and
[0133](0133-a-log-is-read-as-structure-kept-as-it-is-stored.md): a final job's log is stored deflated. Schema 33. No
wire change.

## Context

A daemon of 0.1.5 that had run for a month held a 5.45 GiB `runnerd.db`, 467,355 btrfs extents of it, growing by about
115,000 extents in four days. Of the database, 4.25 GiB was `log_lines`, one row per line of 56,420 jobs' logs, and
583 MiB `events`. The same daemon read about 2.18 TB a day (`rchar`), 70 GB of it from the disk.

Reading its `/proc/<pid>/task/*/io` for 40 s showed that read as bursts of about 480 MB, every 20 s or so, of 4 KiB
reads on the blocking pool: SQLite pages. Three reads that run every few minutes or more scanned every event, 149,000
pages (about 610 MB) each time, on a copy of that database:

- auto sizing's `read::auto_sizing` looked for `profile.changed` events with `json_extract(body, '$.type')`, twice
  per look, every 5 minutes, and once per sizing page a client opened;
- insights' host readings looked for `budget.changed` events by `body LIKE`, over 15 days, whenever the attention
  worker read the insights: once a minute, and again each time the jobs waiting for the host's memory changed;
- `read::slot_jobs`, at each start, looked for `cache.reseeded` events by `body LIKE`.

Every other periodic read searches an index or reads tables of a few MiB (`jobs` 19 MiB, `runners` 25 MiB).

btrfs copies a file's extent on each write unless the file has `FS_NOCOW_FL`. SQLite rewrites its database a page at a
time, so on btrfs every checkpoint fragments the file further and writes more than the pages changed.

## Decision

- **Events of one type are found through an index.** `events_by_type` indexes each event by the type in its JSON, the
  expression `store::event_type!` names (`NULL` for a body that is not JSON, so a broken row cannot fail the
  migration). `read::PROFILE_CHANGES` and `read::RESEEDS` use the same expression, so SQLite searches the index.
- **Host readings are kept beside their events.** The writer stores each `budget.changed` event's reading of the host's
  memory in `host_readings`, in the transaction that stores the event, with the SQL the insights used to run over the
  events (`store::host_reading!`); the migration fills it from the events already stored. Insights read
  `host_readings`. Pruning deletes readings with the events, by the same cutoff.
- **A final job's log is packed.** A job's lines stay rows of `log_lines` while its record may grow, so a running
  log is stored and streamed as before. Once its record has been final for 10 minutes, each prune packs it: it reads
  the rows and deflates them outside a write, then, in one short transaction, stores them as `log_packs` unless the
  job's count of lines moved meanwhile, then deletes the rows a batch at a time with a pause after each. A pack holds
  up to 1024 lines or 256 KiB of text; its data is a layout byte, then each line's number as the gap from the line
  before, its length and its bytes, deflated at level 6 by a pure Rust encoder.
- **One rule for every read.** `store::logs` owns every read of a job's lines: a job with packs reads from them alone,
  one without from its rows, in one snapshot of the database. The writer stores no line for a packed job. So a crash
  between the packs and the last delete leaves rows no read sees, which the next prune deletes.
- **A history's directory is made without copy-on-write.** When `Store::open` creates the directory, it sets
  `FS_NOCOW_FL` on it if it is on btrfs (through `rustix`'s `ioctl_setflags`, without `unsafe`), and on a new empty
  database too: a file takes the flag only while empty, and files made in the directory inherit it. It is best effort.
- **The doctor reports both stores.** `history storage` reports the database's size and file system, and warns while
  btrfs copies it on write, with a fix that makes a copy without copy-on-write (`chattr +C` on the directory, then
  `VACUUM INTO`, with runnerd stopped). `Docker storage` says whether Docker's root is btrfs, ext4 or XFS, or a file
  system in a file on a loop device, and for btrfs what would write less. `observe::storage` reads and words both;
  clients show the checks as they are.

## Consequences

- No periodic read scans `events`. On the copy of the live database: auto sizing's read of `profile.changed` reads 12
  pages in 3 ms instead of 149,000 in 1.5 s; insights' host readings read 1,969 pages (8 MB) instead of 149,000
  (610 MB); `slot_jobs`' reseeds read 6. What the daemon reads of the history each day
  should fall by about two orders of magnitude, an estimate from these page counts not yet measured on a running
  daemon. The migration took 2.5 to 4 s.
- Packing that history's 56,343 final logs took about 5 minutes in one prune, paused as long as it worked: 3.79 GB of
  lines became 682 MB of packs, 5.5 times smaller. The database, 5.37 GB, held 1.70 GB once copied with `VACUUM INTO`.
  The freed pages stay in the file until a `VACUUM`; SQLite reuses them for what it writes next, and the doctor's fix
  for a database btrfs copies on write leaves them out of the copy.
- A finished log is written twice: once as rows, once as packs. Rows of a running job are as before.
- Reading a page of a packed log inflates the packs it spans, at most 256 KiB of text each beyond its last line.
- A history at 33 is not read by 0.1.5, which refuses a newer schema (ADR 0073) as it did 32.
- A database made before this keeps copy-on-write until its owner makes the copy the doctor gives; runnerd does not
  move it.
