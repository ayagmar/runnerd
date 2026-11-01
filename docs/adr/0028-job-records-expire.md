# 0028. Job records expire after `history.jobs`

Status: **Accepted**. Amends [0017](0017-job-history-in-sqlite.md).

## Context

Samples, logs and events were pruned by age; jobs, their steps and runners never were. At about 1 KB per job that is
300 MB a year at 800 jobs a day, on top of the logs and events, and the file only grows.

## Decision

`history.jobs`, default 90 days, three times the stats and log windows. Pruning deletes a finished job's record
once it is that old, with its steps, and its log if `history.logs` is longer; then the runners removed before then
that no job names, with their samples. Queued and running jobs stay whatever their age.

## Consequences

The database stops growing once the oldest job passes `history.jobs`. The file does not shrink: SQLite reuses the
freed pages, and runnerd runs no `VACUUM`. Stats and the job list show no job older than `history.jobs`.
