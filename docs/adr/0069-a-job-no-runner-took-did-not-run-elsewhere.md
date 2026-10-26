# 0069. A job no runner took did not run elsewhere

Status: **Accepted** (2026-10-26). Amends [0059](0059-clients-show-what-core-computes.md).

## Context

The history closes a queued job that left the queue without one of this machine's runners as `elsewhere`, and
clients wrote every such job `elsewhere`. Most of them never ran anywhere: a newer push cancelled their run while
they were queued. On a machine that alone serves its repositories, the history showed dozens of jobs a day as run
on another machine. GitHub gives such a job a `started_at`, its creation time, so its record also had a run time and
a running phase, and its wait of zero counted in the job page's median wait.

A new state would be clearer, but `JobState` is a closed enum on the wire: a 0.0.16 client fails to read a job in a
state it does not know.

## Decision

- A job GitHub completes with no runner name keeps the state `elsewhere` and has no `started_at`. Only a job another
  machine's runner took has one.
- Core words an `elsewhere` job without a start by its conclusion: `cancelled in queue`, or the conclusion itself,
  such as `skipped`; still an ended outcome, never a failure. One with a start, or without a conclusion, stays
  `elsewhere`.
- Migration 14 drops the start of the cancelled and skipped `elsewhere` jobs recorded before, whose start is their
  queue time.
- A job's page compares it with the runs this machine ran only: another machine's runs do not set its medians.

## Consequences

A 0.0.16 client reads the new records and still writes them `elsewhere`. The migration can take a job another
machine's runner started within the second it was queued, then cancelled, for one cancelled in the queue. The state
filter `elsewhere` lists both kinds; the dashboard calls it "Not run here".
