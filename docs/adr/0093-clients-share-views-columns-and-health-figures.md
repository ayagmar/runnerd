# 0093. Clients share their views, table columns and health figures

Status: **Accepted** (2026-11-01). Amends [0062](0062-client-parity-is-checked.md).

## Context

[ADR 0062](0062-client-parity-is-checked.md) made clients show the facts core computes, but left the words around a
fact to each client. Side by side, the dashboard and the TUI listed different views in different orders (the
dashboard hid its records under Caches, the TUI had a view of them), called one table column `Sampled peak`, `MEM`
and `MEM PEAK`, showed a job's age as queued in one and finished in the other, and wrote the overview's four health
figures with different words. `runnerctl job` worded a job's totals a third way.

## Decision

- `runnerd_core::api::View` lists the views in one order (Overview, History, Records, Sizing, Insights, Caches,
  Config) with one name each; `api::Column` names the columns of a table of jobs or of records. The dashboard builds
  its navigation and its tables' heads from them, the TUI its tabs and the heads of its history and records, and
  `runnerctl` the heads of its tables. A client may show fewer views; the TUI has no Insights, Caches or Config.
- `Health::figures` words the four figures at the top of the overview (running, waiting, wait p95 over 24 hours,
  failed over 24 hours) with their notes; the dashboard lays them out as tiles, the TUI on one line.
  `JobMetrics::totals` words what a job used, for the dashboard's page, the TUI's line under the charts and
  `runnerctl job`.
- A job's age in a list is when it was queued, in every client.
- The dashboard's records have a page of their own, `/records`, as the TUI has a view.
- The TUI's tabs and number keys come from `View::ALL`, in core's order, leaving out the views it lacks. What the
  build cache did for a job is worded by `BuildCacheActivity`, in full or as `short_line` for a line with little room.
- `check-boundaries.sh` fails when a client writes a view's name or a fragment of the build cache's wording as a
  string literal; the parity tests of each client assert the order of its views and the names of its columns.

## Consequences

- A name or a figure's words change in one place. A view added to core's list is a client's to draw or to leave out.
- The TUI lost the counts in the Slots and Waiting headings, which the health line says, and says `Queued` where it
  said `Finished`.
- The column check is a test, not a script: a client's own table of something else may use the same word.
