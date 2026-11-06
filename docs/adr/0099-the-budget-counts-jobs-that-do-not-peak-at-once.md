# 0099. The budget counts jobs that do not peak at once

Status: **Accepted** (2026-11-06). Amends [0033](0033-measured-admission.md),
[0055](0055-admission-follows-the-hosts-memory.md), [0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md),
[0097](0097-a-job-may-start-behind-a-barrier-it-cannot-cost.md) and
[0098](0098-a-pool-waits-longer-for-its-first-barrier.md). Adds `[host] pooled_count`, on by default, and an optional
`pooled` object to the budget on the wire. No schema change.

## Context

Under measured admission the budget adds every runner's worst case: the larger of what it reserved and what it uses.
Jobs rarely peak together, so that sum runs far above what they hold. At 02:40 on 2026-11-06 the count read 42.75G with
23.9G in use, and jobs waited behind it.

Discounting a reservation would also discount what a job learns from, and the slice has been hit by real correlated
growth: 19.4G in five minutes on 2026-11-04, from runners whose jobs were not yet known. So a discount has to be small,
aggregate, kept away from what is not yet known, and taken back the moment a reading goes stale.

## Decision

Reservations, `Overran`, `Unpredictable`, prediction windows and stored data stay per runner, as they were. One pure
function, `admission::pooled::count`, gives the aggregate figure. The planner's `Left`, every hypothetical start, the
last check before a start, `over_budget`, the status and `simulate --causal` all read it.

```text
P      = every runner's take + the mirror, seeds and shells     (the plain count)
H_job  = sum over the job's eligible runners of take - use      (linear: copies of a job grow together)
credit = min(sum H - sqrt(sum H²), 6G,
             90% of MemoryMax - slice now - 2G,
             available + growable - margin - total / 4)
count  = P - credit
```

A job here is repository, workflow and name, across profiles.

A runner is eligible once its job is known, this daemon has seen it run 60 s, it has a sample from the last three
intervals without page cache, and its reservation goes by `Job`, `Profile` or `ProfileMax`. Everything else counts in
full: waiting and warm runners, recent starts, stale samples at their caps, adopted runners, `Cap`, `Unpredictable` and
`Overran`, the mirror, seeds and shells. No credit stands without a recent slice sample and host reading, while the host
stalls, under `caps`, or with `pooled_count = false`.

The status keeps `memory_in_use` as the plain count and adds `pooled` (credit, count, runners) while a credit stands. No
new `Basis`. A credit reaches the event log with the budget, by `budget.changed`.

### The constants

- **60 s.** A job's setup says nothing of what it will use: on 2026-11-03 four jobs grew by 14.7G in the 68 s after
  their start was decided.
- **6G.** At 46G the guards bind first, and 4G, 6G and 8G come out alike. At 32G over 72 hours 6G beats 4G (mean run 6m52s
  against 7m03s) and 8G adds nothing.
- **2G under the mark.** The credit leaves that beside what the slice holds, so a start under it does not meet the mark.
- **A quarter of the host's memory.** A pooled count spends what the overcount left the host, and sockets start to
  throttle under about 20G of 62G available. Margin and quarter come to 19.4G, 21.3G with the margin raised. An allowance
  of 0 or 6G costs 6 more minutes under 20G a day at 46G; a quarter costs none, for 4.6 job-hours more of host waits than
  6G.

### Barriers

`Left` reads the pooled count, `Drained` does not change, and rule 4 of 0097 still goes by caps. A barrier can always end:
a drain leaves what it always did, whatever the credit does. Waits and served marks are untouched by a credit.

A credit breaks 0097's promise as written. Room can come with no runner ending: a runner shrinks, the credit grows, and an
owner that needed an end 100 minutes away fits at minute 20, behind a job admitted because it would be gone by then.

Neither strict no-backfill nor an amended promise is needed. Taking an eligible runner out never raises the credit and
frees at most what it took, and a start or a warm runner's stop moves the count byte for byte, so `Left::take` and
`Left::give` stay exact. So `Budget::credit_room`, 6G less the credit, is added to what is left when `Backfill` works out
when room gathers. That time is then the earliest the owner could start, and 0097's first promise stands as it was. With
the credit left out of it, 24 small-world owners start late. Against strict, it waits the same to within noise (the
second table), and against leaving the credit out it keeps the promise exact for at most 11 s of mean run at 32G.

### What `simulate --causal` shows

A replay with a `Trace`: each job uses its own samples shifted to its replayed start, is known only when the daemon could
name it, holds its full reservation for a minute, and runs through the real planner with barrier, backfill, CPU budget and
host bound. The live config at 46G and at 32G, 24 hours before the backup of 2026-11-06, unless the row says otherwise.
Slice overhead is the history's 99th percentile, 7.75G, carried as a constant. Run and jvm columns are means; "jvm" is
runs that used a jvm profile.

| Slice | Credit | Run | jvm | jvm worst | Host wait h | Slice wait h |
|---|---|---|---|---|---|---|
| 46G | none | 7m19s | 11m31s | 26m18s | 36.9 | 2.7 |
| 46G | 6G | 6m44s | 10m01s | 26m23s | 22.7 | 1.4 |
| 46G, 72 h | none | 6m27s | 9m53s | 32m01s | 41.3 | 2.6 |
| 46G, 72 h | 6G | 6m03s | 9m04s | 29m47s | 28.8 | 1.7 |
| 32G | none | 11m14s | 18m43s | 54m16s | 12.8 | 105.1 |
| 32G | 6G | 8m16s | 13m13s | 34m52s | 5.1 | 61.3 |

| 32G, 6G credit | Run | jvm | jvm worst | Host wait h | Started behind |
|---|---|---|---|---|---|
| This rule | 8m16s | 13m13s | 34m52s | 5.1 | 45 |
| Credit not counted | 8m08s | 12m56s | 35m09s | 6.3 | 58 |
| Strict, nothing behind | 8m24s | 13m19s | 37m31s | 7.1 | 0 |

| Hazard | 46G none | 46G 6G | 46G cold none | 46G cold 6G | 32G none | 32G 6G | 32G cold 6G |
|---|---|---|---|---|---|---|---|
| Slice most | 36.5G | 37.1G | 39.3G | 37.0G | 30.9G | 32.6G | 33.9G |
| Past 90%, episodes / time | 0 | 0 | 0 | 0 | 11 / 2m | 24 / 9m | 68 / 29m |
| Past `MemoryMax` | 0 | 0 | 0 | 0 | 0 | 2 / 93 s | 13 / 3m16s |
| Host under 20G | 34m | 32m | 46m | 43m | 36m | 41m | 52m |
| Host under 6.2G | 0 | 0 | 0 | 36 s | 14 s | 0 | 30 s |
| Host's lowest | 9.9G | 7.5G | 10.5G | 4.4G | 5.7G | 8.0G | 3.4G |

"Cold" gives every run its job's heaviest samples: a dependency bump that leaves every cache cold at once. The 32G
crossings go with the 99th-percentile overhead, which is mostly reclaimable page cache: at 4.4G overhead no 6G row at 32G
passes `MemoryMax`, and at 0.8G none passes 90%.

Runner use alone peaks near 25G at 32G.

## Consequences

- At the live 46G no replay row crosses 90% or `MemoryMax`, even cold, with the mean run 8% shorter and host waits 38%
  shorter. At 32G, standing in for contention, the mean run is 26% shorter and the worst jvm run 36%.
- The cost is the host's. A cold window takes `MemAvailable` lower: over 72 hours its lowest falls from 9.1G to 2.7G with
  47 s under 6.2G, where oomd may act once swap is full. The replay's desktop does not react, so a real one would swap
  first, and a real one may also take more.
- The credit falls often, about 7000 times a day, and about 100 of those leave the count past a budget it was within. No job
  stops for that: starts wait until the count fits, a waiting runner past the budget stops as it always did, and the
  running ones end as they would.
- A replay holds run times fixed, so memory pressure never slows a job, and it has no warm runners, reseeds, restarts or
  stall readings. It cannot show what throttling under 20G costs a job. `pooled_count = false` gives back the plain count
  and its plans exactly.
- Doctor does not call the function: it reads the slice and the mirror with no runner's reservation to pool.
