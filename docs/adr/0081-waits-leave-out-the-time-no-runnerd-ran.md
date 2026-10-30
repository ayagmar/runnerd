# 0081. Waits leave out the time no runnerd ran

Status: **Accepted** (2026-10-30). Amends [0073](0073-no-compatibility-before-0-0-19.md), where it says how the
history's schema changes.

## Context

A job's wait counted from when GitHub queued it to when it started. Jobs queued while the machine was off waited for
hours that way: on the owner's host, a light pool's p95 wait was 8h 58m, and one job waited 2h 55m, all of it
before runnerd started. Sizing read those waits as slots too few, auto sizing added slots for them, and insights
said the repositories' waits had grown. #150 counted waits from no earlier than the history's start
(`history_kept`), which covers jobs queued before runnerd first ran, but not a later restart, nor a job seen queued
before the daemon stopped and started after it came back.

Waits were counted in five places: sizing's waits and starts (Rust), insights (Rust), and the dashboard's activity
and `GET /v1/stats` (SQL). Only the first two had #150's clip.

## Decision

- **What a job waited through.** Each job keeps `unseen`, the milliseconds of its wait that passed while no runnerd
  ran (schema 16). When the daemon opens the history, the time from the history's last sign of life (its newest host
  sample or event) to now is downtime. Every job still queued gets the part of it after its queueing; a job queued
  before now that this daemon records later gets the same. Without a sign of life, as in a new history, nothing is
  added: the history's start already clips those waits. Host samples are stored a minute at a time, so after a crash
  up to a minute more counts as downtime. A runner that outlived the last daemon may take a queued job while none
  runs: when GitHub's start time first says so, the part of the downtime after it comes off again. GitHub's listing
  lag and a paused repository's time are not downtime, and stay in the wait.
- **One wait.** `sizing::Wait::counted` is the wait sizing, auto sizing, insights, the dashboard's activity and the
  stats count: from queued, or from the history's start if that came later, to started, less `unseen`, at least 0.
  Activity calls it; stats sums the same expression in SQL (`read::COUNTED_WAIT`), and one test feeds one history
  through activity, insights and stats. Sizing still also takes out what a job waited for the host's memory.
  Arrivals keep `queued_at`: `runnerd simulate` and warm runners model the queue as GitHub made it.
- **A job's own wait** stays queued-to-started, as GitHub shows it. `Timeline.unseen` (seconds, default 0) carries the
  part no runnerd saw, and core words it (`7h 00m of it while runnerd was not running`) for the job pages of the TUI,
  the dashboard and `runnerctl job`.
- **Schema changes are steps.** `store::MIGRATIONS` lists them from version 15, each with the version it brings a
  history to, each in its own transaction. A new history still gets `schema.sql`. A test brings 0.0.22's schema 15
  (a frozen copy in `store/tests/schema-15.sql`) forward and compares every table's columns and every index with a
  new history's, so a `schema.sql` change without its step fails. A read-only look (`runnerd doctor`, `simulate`)
  cannot migrate: on an older history it adds temporary views with the missing columns at their defaults, and empty
  temporary tables for missing tables, and reads it as the current version.

## Consequences

- Waits from before schema 16 keep `unseen` at 0 and read as before; nothing is backfilled from host sample gaps.
- `Started.waited`, the stats' `queue_seconds`, insights' waits and sizing's waits change for jobs queued while
  runnerd was down. A v0.0.22 client shows the same job waits as before, since it ignores `Timeline.unseen`.
- The upgrade is one-way: a 0.0.22 daemon refuses a schema 16 history as newer.
- A job this daemon first learns of from its runner's claim, never having seen it queued, keeps any downtime in its
  wait: it gets its queue time from GitHub after the downtime was handed out.
- A job queued before the history's start and still queued when pruning raises `history_kept` past a downtime would
  have that downtime taken out twice; its wait floors at 0. Pruning only raises it past jobs weeks old.
