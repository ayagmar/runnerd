# 0159. Time the host slept is downtime, not waiting

Status: **Accepted** (2026-11-23).

## Context

A laptop that runs runnerd sleeps. Linux stops `CLOCK_MONOTONIC` while it is suspended, and tokio's timers, the
governor's readings and the idle timeout go by that clock. The wall clock jumps on, and the waits, the store and a
runner's age go by it. Left alone, a sleep of hours read as follows:

- hours of waiting for every queued job, which sizing and insights count, and a wait cause that held the whole time;
- a host sample from before the sleep, read as fresh, so a plan right after the wake admits runners on stale memory
  and CPU figures;
- idle runners whose sessions GitHub ended while the host slept, still counted as covering a queued job;
- a running job that GitHub failed because its runner went silent, read as a failure of its code: a flaky or slower
  insight and a red notification;
- the CPU of the first sample after the wake, divided by the hours asleep, and a drive window stretched across them.

A wall clock stepped forward by a time daemon, as on a laptop that booted with a wrong clock, looks the same to the
wall clock alone, and must change nothing.

## Decision

- A sleep is a rise of `CLOCK_BOOTTIME` minus `CLOCK_MONOTONIC`, read through rustix (`time::suspended_ms`), of
  `SLEPT_AFTER_MS` (30 s) or more since the last look. A step of the wall clock does not move it, and a shorter pause
  is ignored. The engine looks first in each tick and at the start of each pass; the sampler looks on each round.
  Nothing persists the reading: a daemon that starts takes it as it finds it, and the time before its start is the
  store's downtime (ADR 0081).
- On a sleep the engine:
  - sends `Write::Downtime { from, until }` on the system clock. In one transaction the store adds the sleep to
    `unseen` of the jobs queued across it, sets `jobs.slept` on the running ones, ends the session at `from` and opens
    one at `until` (ADR 0135). The writer keeps the downtimes of the last six days in memory, so a job recorded later
    that queued or started before one leaves it out too;
  - closes the open waits and causes at `from`, so the sleep is in neither (ADR 0154);
  - clears the governor's readings and the growth count's looks, and ends each pool's waits;
  - forgets every repository's listing and makes it due now, with its runners;
  - stops each idle runner that was up before the sleep (`StopReason::Slept`), and counts none of them as covering a
    queued job.
- Until the engine has settled, no runner starts, and neither a start failure nor a runner that leaves without a job
  counts toward a pool's backoff. It settles once a listing of queued jobs answered and, if the host was sampled
  before the sleep, a host sample from after the wake is in. A host the sampler cannot read settles on the listing
  alone. `engine/sleep.rs` owns this state as one sealed value; `engine/wake.rs` adds the engine's side.
- The sampler moves its CPU baselines forward by the sleep, so the next figure covers the time awake, and ends the
  drive's window at the last reading before it. `disk::Windows::add` also starts a new window after a gap longer than
  a window, which keeps out a sleep the sampler missed.
- A job that failed or was lost with `slept` set reads `host slept`, kind `Failed`, and its failure line says the host
  slept while it ran, which ended its runner's session with GitHub. A runner fault its log shows still reads as one.
  Insights leave such runs out of flaky and slower, and the notification line carries the failure line. The automatic
  re-run (ADR 0153) is unchanged: it follows `runner_fault` only.
- `jobs.slept` is schema 29, `INTEGER NOT NULL DEFAULT 0`. `JobSummary` and `TimelineBar` carry `slept`, skipped when
  false and read as false when absent, so 0.1.5 and this version read each other's JSON.

## Consequences

- A job's run time still includes a sleep it ran across. The predictor's peaks and CPU-throttle shares from such a run
  are not corrected; the run is marked, so insights skip it.
- The followed jobs' `LOST_AFTER` goes by tokio's clock and runs late by the sleep, which is harmless.
- After a wake, starts wait one listing and one sample, a few seconds on a host that samples every 5 s.
