# 0071. Auto sizing lowers every hour, by up to half

Status: **Accepted** (2026-10-26). Amends [0039](0039-auto-sizing.md) and [0049](0049-auto-sizing-without-hand-tuning.md)
on `[sizing] interval` and `max_step`.

## Context

`interval` (24 hours) and `max_step` (25%) kept a lowering slow because a lowering that went too far had no way back
but the next raise: each run killed at the new cap raised it one step, so a wrong lowering cost a failed job per
step, and a profile far above its need took a week to come down. [0070](0070-predictions-follow-recent-runs.md)
changed that. A lowering goes no lower than the largest need in each job's last 60 runs, plus the margin, and never
to a cap a run in them met; and the first run killed at a lowered cap, or 3 failures in a row on two branches,
takes the profile straight back to the cap it came from, with a floor there for 30 days. The day between lowerings
no longer protects anything: a wrong lowering costs one job whenever it comes. What it still costs is memory held
for jobs that no longer need it, and every queued job waits for its repository's largest reservation (0033).

## Decision

- `interval` defaults to 1 hour, the same as `raise_interval`: a profile can be lowered at the next look an hour
  after its last change.
- `max_step` defaults to 50: a lowering of `memory`, and any change of `cpus`, moves by at most half. A 14G profile
  whose jobs need 8G gets there in one change instead of three days.

Both stay settable; a host that prefers the old pace sets `interval = "24h"` and `max_step = 25`.

## Consequences

- A profile reaches its jobs' recent need within hours of the evidence, not a week.
- A lowering that went too far still costs one failed job, now possibly sooner. A job heavy less often than once
  in 60 runs is the one that pays it; 0070's 30-day floor stops it from paying twice.
- A `cpus` raise also moves by up to half, so a profile short of CPUs catches up in fewer looks.
