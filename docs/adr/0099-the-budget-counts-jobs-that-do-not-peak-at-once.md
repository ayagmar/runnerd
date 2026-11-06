# 0099. The budget counts jobs that do not peak at once

Status: **Accepted** (2026-11-06). Amends [0033](0033-measured-admission.md),
[0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md),
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
function, `admission::pooled::count`, gives the aggregate figure. Against `MemoryMax`, the planner's `Left`, every
hypothetical start, the last check before a start, `over_budget`, a reseed's room, a shell's refusal, the status and
`simulate --causal` read it. The host's limit reads the plain count.

```text
P      = every runner's take + the mirror, seeds and shells     (the plain count)
H_job  = sum over the job's eligible runners of take - use      (linear: copies of a job grow together)
credit = min(sum H - sqrt(sum H²), 6G,
             90% of MemoryMax - slice now - 2G,
             available + growable - margin - total / 4)
count  = P - credit                                             (what MemoryMax reads)
```

A job here is repository, workflow and name, across profiles.

A runner is eligible once its job is known, this daemon has seen it run 60 s, it has a sample from the last three
intervals without page cache, and its reservation goes by `Job`, `Profile` or `ProfileMax`. Everything else counts in
full: waiting and warm runners, recent starts, stale samples at their caps, adopted runners, `Cap`, `Unpredictable` and
`Overran`, the mirror, seeds and shells. No credit stands without a recent slice sample and host reading, while the host
stalls, under `caps`, or with `pooled_count = false`.

The status keeps `memory_in_use` as the plain count and adds `pooled` (credit, count, runners) while a credit stands. No
new `Basis`. A credit reaches the event log with the budget, by `budget.changed`.

### The host reads the plain count

The overcount is the host's margin. The guard reads one moment, while the risk is a desktop burst after runners are in:
the host study saw 10G in 43 s. Runners let in on a credit cannot be taken back, so a host limit that reads the credit
spends a buffer it needs later. Sliding the desktop's recorded memory against the jobs shows it, 46G, 72 hours, 18 shifts
from 5 minutes to 12 hours, a credit of 6G:

| Host limit reads | Warm: shifts under 6.2G | Warm lowest | Cold: shifts under 6.2G, worst | Cold lowest |
|---|---|---|---|---|
| No credit at all | 0 of 18 | 7.53G | 11 of 18, 56 s | 2.82G |
| The credit in full | 0 of 18 | 7.28G | 12 of 18, 135 s | 1.18G |
| The credit less 3G | 1 of 18, 34 s | 2.03G | 11 of 18, 118 s | 2.82G |
| The plain count | 0 of 18 | 7.53G | 11 of 18, 56 s | 2.82G |

What each costs, summed over the 6, 12, 24 and 72 hour windows, against no credit:

| Host limit reads | 46G host hours saved | 46G mean run saved | 32G host hours saved | 32G mean run saved |
|---|---|---|---|---|
| The credit in full | 40.2 of 152.2 | 8.2% | 9.4 of 32.3 | 21.6% |
| The credit less 3G | -12.6 | 1.6% | -2.0 | 20.6% |
| The plain count | -23.7 | -0.6% | -5.1 | 20.1% |

Only the plain count tracks the host's lows without a credit. Less 3G buys no host hours at 46G, and it went under 6.2G
warm where the others did not. So the credit stands against `MemoryMax` alone. What it gives up is the gain at 46G: there
the host binds first, and what the credit lets past the slice's budget waits for the host instead, so host hours go up by
more than slice hours go down. The gain under slice contention stays. Spending host margin for CI time would be a new
decision, to take with a live swap series the backup does not have.

### The constants

- **60 s.** A job's setup says nothing of what it will use: on 2026-11-03 four jobs grew by 14.7G in the 68 s after
  their start was decided.
- **6G.** At 46G the host binds first, and 4G, 6G and 8G come out within seconds. At 32G no one of them leads: over the four
  windows the mean runs differ by 13 s at most. 8G adds a `MemoryMax` crossing over 6 hours that 6G does not, so 6G stays.
- **2G under the mark.** The credit leaves that beside what the slice holds, so a start under it does not meet the mark.
- **A quarter of the host's memory.** A credit lets the slice fill past what the plain count allows, so none stands on a host
  near throttling: sockets start to throttle under about 20G of 62G available. Margin and quarter come to 19.4G, 21.3G with
  the margin raised.

### Barriers

`Left` reads the pooled count, `Drained` does not change, and rule 4 of 0097 still goes by caps. A barrier can always end:
a drain leaves what it always did, whatever the credit does. Waits and served marks are untouched by a credit.

A credit breaks 0097's promise as written. Room can come with no runner ending: a runner shrinks, the credit grows, and an
owner that needed an end 100 minutes away fits at minute 20, behind a job admitted because it would be gone by then.

Neither strict no-backfill nor an amended promise is needed. A credit is at most 6G, whatever the guards do when a runner
ends, and a runner's end gives back at most what it counts. So the room after any runners leave is at most what is left, plus
6G less the credit, plus what they took. `Budget::credit_room`, 6G less the credit, is added to what `MemoryMax` leaves when
`Backfill` works out when room gathers, and the time it finds is the earliest the owner could start. 0097's first promise
stands as it was. A start or a warm runner's stop moves the count byte for byte, so `Left::take` and `Left::give` stay
exact. With the credit left out of it, 24 small-world owners start late. Against strict it runs 19 s shorter at 32G (the
second table), and leaving the credit out gains 1 s at most.

### What `simulate --causal` shows

A replay with a `Trace`: each job uses its own samples shifted to its replayed start, is known only when the daemon could
name it, holds its full reservation for a minute, and runs through the real planner with barrier, backfill, CPU budget and
host bound. Every figure here is from this command on the backup of 2026-11-06, with `--memory-max` and `--since` as the row
says, plus `--cold`, `--priority`, `--shift`, `--host-hold` and `--guard-overhead` where it says so:

```sh
runnerd simulate --causal --cpus 32 --memory-max 46G --since 24h \
  --move 'ayagmar/fretboard:backend / check:backend:static=jvm-heavy' \
  --move 'ayagmar/fretboard:backend / check:backend:migrations=jvm-small' \
  --move 'ayagmar/fretboard:backend / check:backend:upgrade=jvm-small' \
  --move 'ayagmar/fretboard:contracts / check:contracts:breaking=jvm-small' \
  --move 'ayagmar/fretboard:contracts / check:contracts:lint=jvm-small'
```

The moves put those jobs in the pools they run in now. The config is the live one: `jvm` 13.25G with 4 slots, `jvm-heavy`
11G with 3, `jvm-small` 1.75G with 6, `node` 9.75G with 10, `light` 1.75G with 8, `cpu_overcommit = 1.5`. `--slots jvm=6`
changes no figure below. Overhead is the window's 99th percentile, 7.75G at 24 hours, carried as a constant. "jvm" is runs
with a job in that profile.

| Slice | Credit | Run | Run worst | jvm | jvm worst | Host wait h | Slice wait h |
|---|---|---|---|---|---|---|---|
| 46G, 24 h | none | 6m59s | 53m09s | 10m27s | 28m39s | 38.3 | 6.1 |
| 46G, 24 h | 6G | 7m02s | 53m09s | 10m34s | 28m53s | 44.6 | 2.0 |
| 46G, 72 h | none | 5m51s | 52m35s | 8m24s | 31m46s | 52.9 | 6.7 |
| 46G, 72 h | 6G | 5m53s | 52m53s | 8m28s | 31m46s | 59.6 | 2.6 |
| 32G, 24 h | none | 10m51s | 1h01m | 18m35s | 43m38s | 9.5 | 120.6 |
| 32G, 24 h | 6G | 7m57s | 53m03s | 12m56s | 28m39s | 9.6 | 71.3 |
| 32G, 72 h | none | 8m07s | 1h01m | 12m38s | 1h01m | 8.7 | 141.0 |
| 32G, 72 h | 6G | 6m31s | 56m27s | 9m43s | 38m00s | 11.5 | 83.5 |

Without the moves, as the history recorded the pools, 46G over 24 hours goes from 8m57s to 9m09s, and host waits from 103.7
to 138.9 hours: the same trade, by more.

| 32G, 24 h, 6G credit (`--priority`) | Run | Run worst | Host wait h | Slice wait h | Started behind |
|---|---|---|---|---|---|
| This rule | 7m57s | 53m03s | 9.6 | 71.3 | 74 |
| Credit not counted | 7m58s | 53m03s | 8.8 | 71.7 | 70 |
| Strict, nothing behind | 8m16s | 55m58s | 9.0 | 67.3 | 0 |

24 hours; past a line is episodes / time / worst:

| Hazard | 46G none | 46G 6G | 46G cold none | 46G cold 6G | 32G none | 32G 6G | 32G cold none | 32G cold 6G |
|---|---|---|---|---|---|---|---|---|
| Slice most | 37.0G | 35.9G | 42.8G | 40.1G | 29.6G | 32.3G | 33.4G | 34.9G |
| Past 90% | 0 | 0 | 1 / 12 s / 1.4G | 0 | 3 / 78 s / 0.8G | 24 / 5m07s / 3.5G | 92 / 23m / 4.6G | 141 / 41m / 6.1G |
| Past `MemoryMax` | 0 | 0 | 0 | 0 | 0 | 2 / 36 s / 0.3G | 8 / 66 s / 1.4G | 24 / 3m54s / 2.9G |
| Host under 20G | 29m | 30m | 73m | 70m | 32m | 41m | 71m | 83m |
| Host under 6.2G | 0 | 0 | 0 | 0 | 0 | 0 | 14 s | 14 s |
| Host's lowest | 12.3G | 12.3G | 7.8G | 7.8G | 9.5G | 9.0G | 6.0G | 4.8G |

"Cold" gives every run its job's heaviest samples, placed so the run reaches the peak in the time it has: a dependency bump
that leaves every cache cold at once. Over 72 hours at 46G cold the slice reaches 41.7G with no credit and 43.4G with 6G, past
90% for 4 s and 8 s, by 0.3G and 2.0G, and neither reaches `MemoryMax`; the host's lowest is 2.8G either way, 34 s and 30 s
under 6.2G.

A slice can read low when a start is decided and high once the job runs, so the guards and the hazards take separate figures.
With the guards at the median overhead, `--guard-overhead 800M`, and the hazards at the 7.75G, 32G over 24 hours goes past 90%
for 19m12s, by 4.6G, in 44 episodes against 2 with no credit, and past `MemoryMax` twice, for 16 s, by 1.4G, against never;
cold, past `MemoryMax` 44 times for 10m05s, by 2.7G, against 12 times for 1m11s. At 46G the same split changes nothing: the slice
guard does not bind there. The overhead is mostly reclaimable page cache, so these are the worst reading of a figure the
replay cannot follow.

## Consequences

- A slice whose `MemoryMax` is what binds gets the gain: at 32G the mean run is 27% shorter over 24 hours and 20% over 72, slice
  waits 41% shorter, and the worst jvm run a third or more.
- At the live 46G the credit changes little. The mean run stays within seconds, slice waits fall by about 4 hours a day and host
  waits grow by about 6. No warm row crosses 90%, `MemoryMax` or 6.2G, and the host's lows follow the plain count's, cold
  or not.
- The risk is the slice's. At 32G the credit adds crossings of 90% and of `MemoryMax`, more so cold or with a slice that reads
  low at a decision (the figures above). Those rest on the overhead carried, and the replay cannot say how much of it the
  kernel would reclaim first.
- The credit falls often, about 7000 times a day. At 32G about 190 of those leave the count past `MemoryMax`, at 46G one. No job
  stops for that: starts wait until the count fits, a raised waiting runner past the budget stops as it always did, and the
  running ones end as they would.
- A replay holds run times fixed, so memory pressure never slows a job, and it has no warm runners, reseeds, restarts or
  stall readings. Its desktop does not react to a fuller slice, so a real one would swap first. It cannot show what throttling
  under 20G costs a job. `pooled_count = false` gives back the plain count and its plans exactly.
- Doctor does not call the function: it reads the slice and the mirror with no runner's reservation to pool.
