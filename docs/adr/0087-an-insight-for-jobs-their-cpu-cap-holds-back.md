# 0087. An insight for jobs their CPU cap holds back

Status: **Accepted** (2026-10-30). Extends [0051](0051-insights-and-notifications.md).

## Context

Since 0079 each job record keeps its CPU quota's throttled periods, and auto sizing reads a run as held back by its
cap when the quota throttled it in 15% (`HELD`) of its run or more. Only `runnerctl sizing` says anything with it,
and only about a raise auto sizing makes or cannot make. Under `caps` admission auto sizing raises `cpus` only on
averages, never for throttling, and with auto sizing off nothing raises them, so a job the cap slows on every run
shows nowhere the owner looks unless they open each job's page. Insights already name the jobs near their memory
cap and the repositories memory pressure slowed.

## Decision

- **One insight per job**, `cpu-held`, not per repository: auto sizing reads held runs per job, `cpus` belong to a
  profile, and one held job among a repository's many would be diluted in a repository's count. Memory pressure is
  per repository because the host, not the job, is short.
- **The same rule as sizing.** `sizing::Throttling::held` is the one function both read. A job's runs count when
  they succeeded, as sizing reads only those, in the profile and at the CPU cap of its last such run that measured
  throttling, its last 10 of them in the window. The insight holds when 3 or more of those were held back: the
  count that raises `cpus` under `measured` admission (`sizing::auto::RAISE_RUNS`). Runs at a cap the profile no
  longer has say nothing: after a raise, the next runs say whether the new cap holds them.
- **Unknown is not free.** A run without a throttling figure, from before schema 17 or on a kernel without the cpu
  controller, counts neither as held nor in the runs it is held in. A job all of whose runs lack it has no insight.
- **What the owner can do**, worded by core:
  - under `measured` admission with auto sizing, auto sizing raises `cpus` while each raise helps: a notice;
  - when the raise that brought the profile to its `cpus` did not make the job faster or less throttled
    (`sizing::auto::unhelped_raise`, the check auto sizing makes before raising), the insight says so from which
    `cpus`, as this job's evidence only: its runs ask for no further raise. It does not say auto sizing stopped:
    auto sizing may still raise the profile for another job whose raise helped, or for runs that average 70% of
    the cap, and `runnerctl sizing` says which. A warning;
  - with auto sizing off under `measured`, or under `caps` whatever auto sizing does, the `cpus` to set, one
    `max_step` up as auto sizing would step them; under `caps` it says each CPU takes budget and that
    `runnerctl sizing` shows whether it has room: a warning.
- **A fallback kind.** `InsightKind` gains `Unknown`, which a client reads a kind from a later release as, so it
  still shows the list and the insight's sentence. The daemon never sends it.

## Consequences

- v0.0.23's `InsightKind` has no fallback: a v0.0.23 `runnerctl` or TUI talking to a newer daemon fails to read
  `GET /v1/insights` and `GET /v1/digest` while one of these insights holds, and a webhook reader built on
  v0.0.23's types fails to read a notification that carries one. `crates/wire-compat` accepts the break for
  `insights`, `digest` and `notification`. The dashboard ships with the daemon and is not affected. From the next
  release on, a new kind reads as `Unknown`.
- The insight and sizing read the same runs differently in one way: sizing reads runs since the profile's last
  change, the insight its last 10 in the window. Both use the same rule for each run.
- The insight's "set `cpus` to" does not check the slice's CPUs or the CPU budget; `runnerctl sizing` does.
- Rejected: reading the insight's "stopped" from auto sizing's own next look. Insights are pure and computed by the
  insights endpoint, the notification loop and the digest, which also computes them as of 24 hours before, when
  no look of then exists. Each would need the look's inputs (the slice's CPUs, the last changes, the `cpus` the
  daemon ran) and a structured cause on a refused raise, for one clause of one sentence.
