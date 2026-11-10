# 0124. Running jobs are read every poll interval, not every tick

Status: **Accepted** (2026-11-10). Amends [0106](0106-a-tick-reads-its-running-jobs-after-its-starts.md): the running
jobs it reads after a tick's starts are read once every `poll_interval`, not at every tick. No wire, schema or config
change.

## Context

[0106](0106-a-tick-reads-its-running-jobs-after-its-starts.md) reads each running job once a tick, and
[0111](0111-a-tick-is-due-an-interval-after-the-last-one-started.md) made a tick due every `poll_interval`, so the
reads came every 15 s. A tick that comes sooner (a command, a wake-up, a stop's 5 s ticks, and the faster listing that
is planned for an active repository) would read every running job again, multiplying the most expensive line of the
hourly budget for steps and build cache decisions that change over seconds.

## Decision

- **`follow_running` reads the running jobs when the tick's listing began at least `poll_interval` after the listing
  of the tick that read them last, and in the first tick.** `RunningReads` (`engine/running_reads.rs`) owns that one
  due time. A tick that finds them not due skips the read, and does not move the due time. A tick whose starts were cut
  short by a stop reads nothing and notes nothing, as before.
- **Gated:** the reads of running jobs `follow_running` makes after the starts, and with them the build cache decisions
  of running jobs that those reads feed.
- **Not gated:**
  - the pre-plan `describe(unnamed)` in `follow_jobs`, since admission needs a runner's job named before it plans;
  - the reads of the jobs of ended runners whose uploads wait for GitHub's decision, which come once a job has ended,
    at the next tick, and again only if GitHub failed to answer;
  - `follow_closing`'s reads of removed runners' jobs (0109), and the closing of an ended runner's build cache;
  - everything that follows a job's end: Docker's events and the tick's inspects.

## What goes stale, and for how long

Nothing at the default: ticks `poll_interval` apart always read. Only a tick that comes sooner reads less.

- A running job's steps in the history and the dashboard: up to `poll_interval` plus one tick, which at 15 s ticks
  is what it was.
- A running job's build cache write decision: the same bound. The job's own saves already wait for it.
- A job's end: not at all. Its runner's container stops, Docker says so, the runner is released, its job closes by the
  closing reads and its uploads by their own read, every tick.
- A stop's 5 s ticks read the running jobs every third tick.

## Consequences

- On the fakes in `workload_figures` (a 40-job burst every 5 minutes for 30 minutes, 8 slots), with something waking
  the engine every 5 s: 1,940 job reads an hour before and 950 after, against 954 at the default 15 s tick, which this
  leaves as it was. Queued and runner listings are untouched at 702 and 704 an hour.
- A later change to the listing pace can make ticks faster without multiplying the running jobs' reads.
- The pace test `running_jobs_are_read_every_poll_interval_whatever_the_ticks` holds four running jobs while ticks
  come every 5 s and finds a round of reads every 15 s.
- The invariants' model ticks every 5 s; `no_job_stays_running` still holds.
