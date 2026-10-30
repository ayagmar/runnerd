# 0075. `cpus` come down only under `caps` admission, and only under a cap no run's bursts met

Status: **Accepted** (2026-10-30). Amends [0068](0068-cpus-follow-run-averages.md), where it states the lowering.

## Context

0068 lowered a profile's `cpus` toward twice its heaviest job's p95 average, and accepted that "a short parallel
phase runs slower than it could". On the owner's host, 2026-10-26, that cost turned out to be large, and the
lowering did not stop. JVM went 31 → 16 → 10 → 9 CPUs and Node 22 → 17 → 9 → 8 → 7 in under a day. Each lowering
read averages measured under the cap before it, and those averages fell with the cap: the Android quality gate
averaged 5.4, 4.9, 4.4 and 4.1 CPUs at 31, 16, 10 and 9. Its CPU time stayed at about 2,130 CPU-seconds, and its
peak sat on each cap: the job spent more time held, so its average dropped, so the next target dropped. At 9 CPUs it
ran about 90 seconds (20%) slower than at 16.

The cut freed nothing either. The host runs `admission = "measured"`, where a runner with a known job reserves that
job's predicted average, not its cap, and `cpus` bound no slot (`auto::cpu_slots` is `None`). In 2,416 jobs the CPU
budget was why a job waited twice.

## Decision

- Under `admission = "measured"`, auto sizing never lowers `cpus`. Raises are unchanged: a job averaging 70% of the
  cap still gets more.
- Under `caps`, a lowering reads the same runs as before (successful, at the profile's current `cpus`, since its last
  change and since the daemon got them), and holds the profile when any of them peaked at 95% of the cap or more.
  Such a run met the cap in a burst: it ran as fast as the cap let it, so its average says what the cap allowed,
  not what the job needs. Peaks are the highest 2-second interval, so a peak can pass the cap a little; 95% counts
  those as met.
- When none met it, `cpus` come down toward the larger of twice the heaviest job's p95 average and the highest
  peak among those runs, rounded up to whole CPUs, by at most `max_step` a change. A lowering never goes under a
  burst it saw, so the next runs do not meet the new cap, and the one after reads the same peaks and stops there.

0068 rejected peaks for raising, as a parallel build bursts to whatever cap it gets. That is why a peak at the cap
holds a lowering here, and why the peak is only a floor: a job whose bursts reach any cap keeps the cap it has.

## Consequences

- No schema or wire change. The lowering's reason now says the runs' highest peak beside the heaviest average.
- A profile whose builds burst to their cap is never lowered by auto sizing. Lowering it is a hand edit, and a cap
  its bursts meet at the lower value stays there.
- A profile well over every burst comes down to its highest burst, no further. At 31 CPUs the Android gate's bursts
  reached 23.5, so under `caps` it would stop at 24, where 0068 went on to 16 and below.
- Contention can still cost a step: a day in which other programs hold both the average and the bursts of every
  run down looks like a lighter job, as 0068 says.
- Rejected: a fixed floor or a `min_cpus` knob (one more number to tune by hand), and taking the floor from runs at
  an earlier, larger cap (a hand edit, a reload or an undo starts the evidence over, as 0068 decided).
