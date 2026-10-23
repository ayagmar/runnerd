# 0027. Pruning runs beside the writer, a batch at a time

Amends [0017](0017-job-history-in-sqlite.md), which had one thread make every write.

## Context

Samples, logs and events were pruned by age, hourly, on the writer thread, in one statement each. Pruning events
parsed every stored event's JSON to find each repository's latest one, which it must keep: 175 ms per hourly run on
a 525,000-event history, 200 ms after a day off, while log lines, events and samples waited behind it.
`samples_by_at`, an index only pruning used, was more than half the samples table.

## Decision

- Pruning runs on Tokio's blocking pool on its own connection. It deletes at most 2,000 rows per transaction and
  pauses after each as long as the batch took, so the writer, waiting on SQLite's busy timeout, gets the lock in
  between. It writes nothing but deletions. One prune runs at a time.
- Migration 4 adds `announced`, each repository's latest `repository.*` event, which the writer keeps in step in
  the event's transaction. Pruning keeps those events, and a restarted daemon reads only them.
- Samples are pruned by runner through the primary key, and `samples_by_at` is dropped.

## Consequences

The writer waits for one batch at most, not a whole prune. The database has a second writing connection, which only
deletes what `[history]` no longer keeps.
