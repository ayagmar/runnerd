# 0032. Sizing advice from sampled peaks, applied through the config check

## Context

Every profile's `memory` and `slots` are guesses made when the config was written. The job history has what each
job used: its memory and CPU peaks, sampled every 2 s, its container's cap, and how long it waited for a slot. A cap
set far above what jobs use wastes budget that could run more jobs at once. A cap jobs reach stops them: the cgroup
kills the job, or holds it back while it reclaims memory.

Advice has to be safe to follow and has to say how sure it is. A 2 s sample misses a short spike. A profile serves
several workflows, and its rarest job may be its largest. A handful of jobs says little.

## Decision

The daemon computes the advice from the history on each request (`GET /v1/sizing`, `sizing::advise`, pure). Every
client shows the same advice and the same sentence for it (`api::Sizing::advice_line`):

- Percentiles are nearest-rank, so each is a peak a job had, and with fewer than 100 jobs p99 is the largest.
- A job, keyed by repository, workflow and name, needs its p99 peak plus `[sizing] margin` percent (default 15),
  for what sampling misses. With fewer than `[sizing] min_jobs` runs (default 10) its largest peak counts.
- A profile needs the largest of its jobs' needs and of its own p99 plus the margin, rounded up to 256M. No job of it
  gets less than it used.
- Below `min_jobs` jobs a profile gets no advice. A need a tenth or more under `memory` lowers it, and adds as many
  slots as the profile's memory holds now when jobs waited a minute at p95 or the slots were busy half the time. A
  need over `memory` raises it, taking only what `MemoryMax` has spare, and lowers `slots` when it must.
- Cancelled jobs do not count: a cancel stops a job before its peak.

Advice is never written by the daemon. A client applies it on request and after a confirmation: `runnerctl sizing
apply` asks on the terminal or takes `--yes`, the TUI asks for `y`, and the dashboard opens the change in its config
editor, where it is saved like any edit. Each sends the whole file to `PUT /v1/config`, which checks it against
`MemoryMax` as it checks every config. The change to the text is `config::resize_profile`, in core, built for
clients: it replaces the profile's `memory` and `slots` and keeps everything else as written, comments included.
That needs `toml_edit`; `toml` writes a whole new file and drops comments.

## Consequences

The advice reads the history on each request: a scan of the last 30 days' finished jobs and runners, a few
thousand rows. Nothing is cached.

Advice stays conservative where data is thin or skewed: one heavy job holds a profile's cap up, and a profile with
few jobs gets none. Peaks within 5% of their cap are counted and shown, since such a job may need more than it shows.

The advice for a profile is computed as if no other profile changes. Lowering never asks for more than the profile
takes now, so any set of lowered profiles fits. Two raises both take the same spare memory; applying the second is
refused by the check.

The config read and the write are two requests, as in the dashboard's editor: a change made to the file between them
is replaced.

`runnerd-core` without `host` depends on `toml_edit` 0.25, which shares its parser, writer and `winnow` with
`toml`: it is the only new crate.
