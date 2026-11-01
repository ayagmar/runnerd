# 0075. `cpus` come down only under `caps` admission and a cap no run's bursts met; under `measured` the bursts raise them

Status: **Accepted** (2026-10-30), amended by [0079](0079-cpu-weights-and-throttling-as-evidence.md). Amends [0068](0068-cpus-follow-run-averages.md).

## Context

0068 lowered a profile's `cpus` toward twice its heaviest job's p95 average, and accepted that "a short parallel
phase runs slower than it could". On the owner's host, 2026-10-26, that cost turned out to be large, and the
lowering did not stop. JVM went 31 → 16 → 10 → 9 CPUs and Node 22 → 17 → 9 → 8 → 7 in under a day. Each lowering
read averages measured under the cap before it, and those averages fell with the cap: the Android quality gate
averaged 5.4, 4.9, 4.4 and 4.1 CPUs at 31, 16, 10 and 9. Its CPU time stayed at about 2,130 CPU-seconds, and its
peak sat on each cap: the job spent more time held, so its average dropped, so the next target dropped. At 9 CPUs it
ran about 90 seconds (20%) slower than at 16.

The cut freed little either. The host runs `admission = "measured"`, where a runner with a known job reserves that
job's predicted average, not its cap, and `cpus` bound no slot (`auto::cpu_slots` is `None`). With `cpu_overcommit`
set, `cpus` still take some of the CPU budget: an idle warm runner, and a runner whose job has no prediction,
reserve their profile's cap, and a running job counts at its peak once that passes its reservation. In 2,416 jobs
the CPU budget was why a job waited twice.

## Decision

- Under `admission = "measured"`, auto sizing never lowers `cpus`, and raises them on bursts as well as averages:
  3 runs of one job since the last change that peaked at 95% of the cap or more raise it by `max_step`, up to the
  slice's CPUs, as do 3 averaging 70% of it, as before. A running job reserves its average there whatever its cap,
  so for such a job the cap mostly slows its bursts. A profile the old lowering cut climbs back, a step a look, to the
  first cap its bursts do not meet, and stays: without a CPU budget, jvm at 9 CPUs goes 9 → 13 → 19 → 28 at the
  default 50% step, over a gate bursting to 23.5. A burst the slice's CPUs or the CPU budget hold back is not shown as
  a refused raise.
- With `cpu_overcommit` set, a measured raise also has to fit the CPU budget as measured admission holds it at caps:
  every profile's warm runners (each pool's `warm` added up, at most the profile's slots) idle at their caps, and one
  more runner at the largest cap, as one whose job has no prediction reserves. The raise stops at the largest
  `cpus` for which that fits, and the profile keeps its `cpus` when none above them does. On the owner's host (a
  32-CPU slice at 1.5, so 48 CPUs; one warm runner each in jvm, node, light and caches, the last two at 2 CPUs) jvm
  goes 9 → 13 → 17 and node 7 → 10: 17 + 10 + 2 + 2 warm, and one more at 17, are 48. The same check as `caps`
  admission, `slots × cpus`, would refuse every raise there: the owner's 4 jvm and 10 node slots already add up to
  106 CPUs at 9 and 7, and measured admission never reserves them all at their caps.
- Under `caps`, a lowering reads the same runs as before (successful, at the profile's current `cpus`, since its last
  change and since the daemon got them), and holds the profile when any of them peaked at 95% of the cap or more.
  Such a run met the cap in a burst: it ran as fast as the cap let it, so its average says what the cap allowed,
  not what the job needs. Peaks are the highest 2-second interval, so a peak can pass the cap a little; 95% counts
  those as met.
- When none met it, `cpus` come down toward the larger of twice the heaviest job's p95 average and the highest
  peak among those runs, rounded up to whole CPUs, by at most `max_step` a change. A lowering never goes under a
  burst it saw, so the next runs do not meet the new cap, and the one after reads the same peaks and stops there.

0068 rejected peaks for raising, as a parallel build bursts to whatever cap it gets. Under `caps` that still holds:
there a raise takes CPU budget from other slots, so a peak at the cap holds a lowering and is only its floor, and a
job whose bursts reach any cap keeps the cap it has. Under `measured` a raise takes budget only through what is
reserved at caps, which the budget check above bounds, and a build bursting to its cap raises it until the bursts
fit, the budget runs out or the slice does.

## Consequences

- No schema or wire change. The lowering's reason now says the runs' highest peak beside the heaviest average.
- Under `caps`, a profile whose builds burst to their cap is never lowered by auto sizing. Lowering it is a hand edit,
  and a cap its bursts meet at the lower value stays there. Under `measured` it is raised back.
- Under `measured` without a CPU budget, a build that bursts to whatever it gets (one wider than the slice) raises its
  profile to the slice's CPUs. The kernel shares the slice by weight, and admission reserves averages. With a budget,
  the raise stops where the warm runners and one more runner at the largest cap fill it, so idle warm runners never
  hold the whole budget. Profiles earlier in the config take that room first, as any change in a look sizes against
  the budget the changes before it leave.
- A running job's peak still counts once it passes its reservation, and a larger cap lets a burst peak higher. The
  one runner at the largest cap left in the budget is the room for that; a host whose bursts land together can still
  make a queued job wait for CPU.
- A profile well over every burst comes down to its highest burst, no further. At 31 CPUs the Android gate's bursts
  reached 23.5, so under `caps` it would stop at 24, where 0068 went on to 16 and below.
- Contention can still cost a step: a day in which other programs hold both the average and the bursts of every
  run down looks like a lighter job, as 0068 says.
- Rejected: a fixed floor or a `min_cpus` knob (one more number to tune by hand), and taking the floor from runs at
  an earlier, larger cap (a hand edit, a reload or an undo starts the evidence over, as 0068 decided).
