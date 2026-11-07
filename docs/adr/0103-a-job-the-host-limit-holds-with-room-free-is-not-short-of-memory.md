# 0103. A job the host limit holds with room free is not short of memory

Status: **Accepted** (2026-11-07). Amends [0100](0100-an-insight-for-jobs-held-for-the-hosts-memory.md). No wire, schema or
config change.

## Context

The host limit reads the plain count ([0099](0099-the-budget-counts-jobs-that-do-not-peak-at-once.md)), so what runs can
reach it while it holds far less than it reserves. A credit makes that common: it opens the slice, and the next job waits for
the host. In the 0.0.29 sandbox run the log said `the host is short of memory (42.13G available)`, and such a job read
`The host is short of memory`. `host-held` would have told the owner to close programs with 40G free.

Admission is right to hold these jobs: 0099 keeps the overcount as the host's buffer against a desktop burst. Only what it
says about them is wrong.

## Decision

- **The host lacks room when it stalls, or when `MemAvailable`, with what it can still grow by, less the plain margin is under
  what the job needs.** `HostMemory::room` and `HostBudget::short_of` say it, from the wire's reading, so a client words a wait
  as the daemon logs it. The plain margin, not the one in force: a job between the margins has room, and 0102 lets it go when
  the count allows. `host_margin` moves to `api` for this; `admission::host::margin` adds the band on top.
- **A job held with room free says what binds.** `The host has 42.13G of 62G available, but what runs may still grow into it:
  needs 7G; 3.37G of the 49.37G it leaves the slice is free`. It names no programs, as closing them is not what it waits for.
  `Waiting::HostMemory` stays, so `WaitKind`, barriers and 0102 read it as before.
- **The log says the same.** `note_host` logs `what runs reserves … of the … the host leaves the slice`, and keeps `the host is
  short of memory` for a stall or a job the host lacks room for. It logs a change between the two.
- **`host_waited` still counts these jobs.** Sizing takes it off the wait it reads as a wait for the budget, and the host limit
  is what held them. Left out, those spans would read as a slice too small. It stays what its name says: the time the host
  limit held a job.
- **`host-held` hears only of jobs the host lacks room for.** `note_host` gives `Store::host_waiting` those alone, by the plan's
  reading, so with none the insight is silent, and its count leaves out jobs held with room free.

## Consequences

- The longest wait `host-held` quotes can include spans held with room free, as `host_waited` does not tell the two apart. A
  job can pass 30 minutes that way and fire the insight at the first plan the host really lacks room. The insight still only
  speaks while the host lacks room.
- The insight reads the plan's verdict, not its own reading: for up to a plan it can lag a host that just got room, as it can
  lag a job that just started.
- Near the line the log can change between the two wordings as `MemAvailable` moves, at most once a plan.
- The larger margin does not change: `host::short` still reads the plain count, so it can stand while no job is short by this
  rule. That margin guards what runs reaching the limit, whichever way the line is worded.
