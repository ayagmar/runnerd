# 0047. The jobs started and ended in a window, by those times

Status: **Accepted** (2026-10-24).

## Context

The overview's health figures (p95 wait, failures) are meant to cover the last 24 hours. The dashboard first
built them from `GET /v1/jobs`, which filters by when a job was queued and returns at most 500 a page. That missed a
job queued days before it started or ended, kept only the newest 500 on a busy day without saying so, and spent
one summary read per job, on every job event and for each open overview.

## Decision

`GET /v1/activity?since=` (`api::Activity`) reads, from `since` (default 24 hours ago) until now, every job
runnerd's runners started (when, and how long it waited) and how many jobs ended, completed or lost, and how many
of those failed, timed out or were lost, as `GET /v1/stats` counts failures. A job counts by its start and end
times, whenever it was queued. There is no page limit: a start is two integers, and a day of a busy host is a few
thousand.

The overview reads it for the 24 hours up to now, and draws its hourly wait bars as 24 hours counted back from the
end of that window.

## Consequences

- The figures cover every job of the window, so no truncation notice is needed.
- `started_at` and `finished_at` have no index: each read scans the retained jobs once per query. Retention bounds
  that table; an index can follow if a host's history makes it measurable.
- Rejected: following `GET /v1/jobs` pages (hundreds of summary reads per refresh, still by queue time), and
  returning the p95 and hourly maxima ready-made (the client chooses its percentile and buckets, and both stay
  testable without the daemon).
