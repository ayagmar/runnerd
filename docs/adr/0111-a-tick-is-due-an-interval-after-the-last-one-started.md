# 0111. A tick is due an interval after the last one started

Status: **Accepted** (2026-11-08). Amends [0092](0092-idle-removals-and-deferred-starts-stay-off-the-start-path.md) and
[0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md): GitHub is polled every `poll_interval` from a tick's
listing, not its end. No wire, schema or config change.

## Context

The next tick was due `poll_interval` after the last one ended, so everything a tick does after its listing (following
jobs, removals, starts, the running jobs' reads, records, prune) pushed the next listing out by its own length. ADR 0092
chose that so a shorter tick only brings the next poll back to the configured interval, never sooner.

On the owner's host, over 9.3 hours of 0.0.29 with `poll_interval` at 15 s, listings of a repository that saw new jobs
came at least 16 s apart, a median 36 s. GitHub shows a job about a second after it queues, yet runnerd saw cold jobs a
median 13.7 s and p90 25.0 s after their queueing, close to uniform over 0 to 27 s: the poll period was about 24 s, not
15 s. Ticks over 15 s took a median 18.2 s, p90 26.0 s.

## Decision

- **A tick is due `poll_interval` after the last one started its listing**, so the tick's own work no longer moves
  the next listing. The anchor is the listing's start, not the tick's: what a tick collects before it lists (finished
  runners, records, cancelled runs' removals) varies from tick to tick, and counted from the tick's start, a slow
  collect followed by a quick one would bring two listings closer than the interval. A stop's detach, which lists
  nothing, counts from its start.
- **At least 2 s after the last one ended** (`TICK_REST`), or `poll_interval` if that is shorter: a tick that outlasts
  the interval, under a slow GitHub, does not run the next one back to back.
- A stop's ticks, 5 s apart, follow the same rule. A command, a wake-up or a stop still brings a tick sooner, and a
  pass between ticks still leaves the next one when it was due (ADR 0095).

## Consequences

- Two listings are never closer than `poll_interval`, as before, outside commands and stops: at 15 s, 240 an hour at
  most. Ticks shorter than the interval now reach that ceiling, where before they fell short of it by their own
  length. In `workload_figures`, with Docker's events followed, queued-jobs listings went from 200 to 238 an hour and
  running-job reads, one per running job a tick, from 752 to 944. On the owner's host that is well inside GitHub's
  5,000 requests an hour for the token, and when the limit is used up the client sends nothing until its reset
  (`github::Error::RateLimited`), as before.
- A tick whose collect took 8 s, removing a cancelled run's runner, still leaves the next listing 15 s after its own
  (`tests/pacing.rs`).
- On the fakes with four running jobs whose reads take 9 s, listings came 24.9 s apart and come 15.0 s apart now; with
  20 s reads, back to back would be 20.9 s, and the rest makes it 22.9 s (`tests/pacing.rs`).
- Expected on the owner's host: GitHub's queueing to runnerd seeing a cold job from a median 13.7 s to about 9 s. The
  history's `seen_at` measures it.
- Running-job reads are still tied to the tick (ADR 0106), so they remain what a faster cadence costs most.
