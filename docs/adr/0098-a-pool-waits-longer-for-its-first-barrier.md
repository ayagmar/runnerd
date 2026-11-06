# 0098. A pool waits longer for its first barrier

Status: **Accepted** (2026-11-06). Amends [0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md) and
[0097](0097-a-job-may-start-behind-a-barrier-it-cannot-cost.md). No wire, schema or config change.

## Context

With 0096's limit of 120 s a barrier stands for about 4 of 12 hours under contention, even with 0097's passes, and the
median run takes four times as long as with no barrier (the table below, 32G over 12 hours). Most of those barriers stand
for a shortage that would have passed on its own.

A longer flat limit costs a pool of many large jobs. Each runner a pool starts ends its wait, so the next job starts its
limit again. Where younger jobs take what frees up and a run is no longer than the limit, the pool never gets a second
runner beside the first: thirteen 11G jobs running 10 minutes each, in a slice a stream keeps full, wait up to 132 minutes
at a flat 600 s and 44 at 120 s (replayed below).

## Decision

A pool's job waits `PRIORITY_AFTER`, 600 s, before it can own a barrier. Once a barrier has served the pool, its jobs wait
`SERVED_PRIORITY_AFTER`, 120 s, until its queue empties; then it is back to 600 s. A long wait is evidence that a shortage
will not pass by itself, and the mark carries that evidence to the jobs queued behind the first.

`Waits` holds the rule, so `plan` and `admission::simulate` read the same limits through `Barrier::of`, and the replay
commits through the same `own`, `started` and `emptied`. The mark is in memory, like the waits. Nothing changes on the wire,
in the schema or in the config, and an owner has no key to tune, as under 0096.

### Served

The barrier has served a pool when a runner of the pool is in its slot, for a wait whose job owned a barrier in a committed
plan. That takes two steps, because the plan that starts the owner has no barrier in it: the job fits.

- `Waits::own` notes the job when `note_waits` commits a barrier that is its pool's. A status's plan never gets there.
- The note stays when the barrier moves to an older job or falls under an interlock. The job owned a barrier during this
  wait and the room it gathered is what it starts in. It also stays when `still_fits` refuses a start or preparing the caches
  fails, as neither brings a backoff, so the retry counts. A failed registration or Docker start brings one, which clears
  it like any other backoff.
- `Waits::started` turns the note into the mark when the runner is in its slot, where 0096 ends the wait. A proposed start,
  a deferred one or one that failed preparing its caches does not.
- A job that leaves the queue with no runner of ours, cancelled or taken elsewhere, takes its note with it, before the plan
  of the tick whose listing shows it gone. The barrier gave the pool nothing, so the next job waits 600 s.
- A job that passed its limit and started without a barrier ever standing marks nothing. That is the case of a pool held by
  its slots, whose clock ran on while it had none free and for which no room was ever held. Marking it would give the busiest
  pools the short limit for free. At 32G the two readings give the same figures in all three windows, so the reason is
  the rule's meaning and not a measurement.

The mark is the pool's, as 0096's promise is: GitHub may give the runner any job of the pool.

### Queue empties

The mark goes when a listing that succeeded leaves the pool no job, counted as `note_waits` counts its waits: the listing
less the jobs a runner of this machine is known to have taken. A job hidden for a runner no capture named yet still counts,
so a pool's last job keeps the mark until the next listing or capture settles it. The mark then lasts seconds for a pool of
one job, which is right: it had nothing behind the barrier.

- A listing that failed shows no job and says nothing: the mark stays, like the waits. No barrier stands for the pool
  meanwhile, as the plan has no job of it.
- One of several jobs taken elsewhere leaves the mark: the rest of the queue is the same contention.
- A gap in the queue ends the contention as far as the rule knows, even if the next stage of a run queues seconds later. That
  stage waits 600 s. The other reading, a mark that outlives the queue by some time, needs a second constant this history
  cannot set.

### What else ends it

- A pause, a failure backoff and a reseed or image pull clear the mark along with the waits (`note_waits`'s `stopped`). A
  broken pool must not come back early, and a queue that sat through a reseed is not the contention that earned the mark.
- A reload that removes a pool or moves it to another profile prunes the entry, so a new pool cannot inherit it.
- A restart loses it. A pool that was served waits 600 s again, and frequent restarts inside 600 s would keep a job from ever
  holding. That window was 120 s under 0096.

### Memory and CPU

One rule for both, and `MEMORY_PRIORITY_AFTER` and `CPU_PRIORITY_AFTER` are gone. 0096 kept them apart in case they had to
move apart, and the replay says they should not. With `--cpus 32`, as in the tables below, leaving CPU at 120 s changes no
figure, as memory runs out first. Where CPU is what runs out the same case holds as for memory. At 46G with 12 CPUs over 12
hours, all runs mean / p50, runs with a jvm job mean / max, and light's p95:

| CPU limit | All runs | Runs with jvm | light |
|---|---|---|---|
| no barrier | 7m59 / 1m22 | 14m04 / 46m41 | 1m02 |
| 120 s | 10m57 / 5m35 | 15m50 / 51m02 | 8m38 |
| 600 then 120 s | 8m12 / 2m06 | 13m09 / 36m08 | 2m31 |

At 16 CPUs no barrier goes up under the two limits. At 8 the load is far past capacity, and either limit more than doubles
the mean run against no barrier (2h03 and 2h05 against 55m12), which is no case for keeping them apart.

### What the constants rest on

One night of history. The direction holds across windows and budgets; the two numbers do not have that support. At 300 s
flat the review of 0097 found better figures at 30G and worse ones at 32G, and the noise floor between rows is a minute or
two. If a sandbox or live run disagrees, these are two constants in `scheduler::priority`.

### 0096 and 0097 otherwise

Barriers, `drained`, reasons and what jobs say are as 0096 has them. T0 in 0097 is when the wait reached whichever limit
applied, so rule 4 counts from there. Both bounds of 0097 still stand, as neither depends on the limit's length. The limit is
fixed for the whole of a wait: the mark comes only as a wait ends and goes only with an empty queue, so T0 does not move.

An unserved job older than a served pool's job goes on aging behind that pool's barrier and takes the barrier at 600 s, as
any older job does. The shorter limit gives a served pool earlier barriers, not a better place in the order.

`every_world_of_six` now runs a third shape. `TIGHT` and `SHARED` tick at 600 s, so both limits are a tick, as they were
under one limit. `TWO_LIMITS` ticks at 300 s, so a first wait takes two ticks and a served one a tick, and its jobs arrive
together or two ticks apart, to land behind a barrier that went up in between. `TIGHT` and `SHARED` give 0097's counts
unchanged. `TWO_LIMITS`, up to 6 jobs, is 26,051,628 worlds and 2,291,604 passes:

- A run takes what the plan read: no owner started late, whether it kept its barrier or not.
- Estimates read double: 4,220 owners that kept their barrier started late, by a tick, none past the later of its strict
  start and the last end of what passed it. 80 that lost theirs started late, by a tick.
- Jobs behind a barrier hang: no owner was kept out, unless an older job took its barrier.

An owner that lost its barrier now starts late only on estimates that read double. Seed 767, the one generated world 0097
quotes for that case with exact estimates, shows nothing under these limits, nor does any of the first 1,500 seeds, and no
small world does. `an_owner_that_loses_its_barrier_can_start_later_than_under_a_strict_one` pins a 6-job world of the double
case, so the branch that excuses such an owner stays reached on every `cargo test`, and `every_world_of_six` asserts it too.

The start that serves a pool logs one `info!` line, which is how a sandbox run sees the mark. A failed start logs none, even
one that left a container in the slot.

## Replay

The owner's history to 2026-11-06, `simulate --priority`, `--cpus 32 --slots jvm=6`, the live config, with
`check:backend:static` moved to jvm-heavy and the four small fretboard jobs to jvm-small, in the replayed window and in what
it knows beforehand. That is how those pools run now; the backup ran them in jvm. The move took a patch that is not in the
tree. Figures are all runs mean / p50, runs with a jvm job mean / max, light p95, and the p95 of fretboard/jvm, jvm-heavy and
trellis/jvm.

| Slice | Window | Trigger | All runs | Runs with jvm | light | fretboard/jvm | jvm-heavy | trellis/jvm |
|---|---|---|---|---|---|---|---|---|
| 32G | 6h | no barrier | 7m46 / 1m02 | 13m58 / 38m46 | 44s | 19m16 | 31m06 | 24m08 |
| | | 120 s | 8m46 / 2m33 | 14m11 / 42m26 | 6m06 | 19m30 | 19m18 | 22m49 |
| | | 600 then 120 s | 8m07 / 1m42 | 13m51 / 35m13 | 1m35 | 20m40 | 21m34 | 21m50 |
| | 12h | no barrier | 8m16 / 1m13 | 14m54 / 41m39 | 42s | 31m07 | 28m19 | 32m02 |
| | | 120 s | 10m48 / 5m14 | 16m25 / 53m14 | 10m27 | 22m03 | 22m32 | 20m00 |
| | | 600 then 120 s | 8m31 / 1m52 | 14m20 / 37m10 | 1m28 | 21m53 | 20m45 | 20m00 |
| | 24h | no barrier | 8m33 / 1m17 | 16m05 / 42m25 | 40s | 33m29 | 26m57 | 29m36 |
| | | 120 s | 11m23 / 6m51 | 16m47 / 52m38 | 9m20 | 24m58 | 25m53 | 19m55 |
| | | 600 then 120 s | 8m48 / 2m10 | 14m47 / 36m38 | 3m10 | 20m49 | 21m30 | 20m40 |
| 46G | 6h | no barrier | 5m44 / 54s | 7m48 / 24m59 | 26s | 4m00 | 4m57 | 7m06 |
| | | 120 s | 5m45 / 56s | 7m46 / 24m59 | 29s | 4m00 | 4m37 | 7m07 |
| | 12h | no barrier | 5m44 / 1m05 | 7m32 / 24m37 | 27s | 3m31 | 4m57 | 5m24 |
| | | 120 s | 5m44 / 1m07 | 7m31 / 24m37 | 30s | 4m25 | 3m37 | 5m24 |
| | 24h | no barrier | 5m41 / 1m04 | 7m31 / 24m37 | 27s | 3m31 | 4m57 | 5m24 |
| | | 120 s | 5m41 / 1m05 | 7m30 / 24m37 | 29s | 4m25 | 3m37 | 5m24 |

At 46G no barrier goes up under 600 then 120 s in any window, so that row equals no barrier on every figure.

At 32G a barrier with passes stood 1h14m of 6 hours, 1h15m of 12 and 1h47m of 24, against 2h36m, 3h49m and 4h07m at 120 s.
Passes fell from 396, 535 and 433 to 152, 146 and 149.

Runs with a jvm job are what hold a pull request, and this is the only trigger of the three that beats no barrier on their
mean and their maximum in all three windows at 32G. It costs all runs 15 to 21 seconds on the mean against no barrier, where
120 s costs 1m00 to 2m50. Light's p95 is 1m28 to 3m10 against 6m06 to 10m27. fretboard/jvm's p95 is 1m24 worse than no
barrier over 6 hours and 9 to 13 minutes better past that. No trigger wins every column: 120 s has jvm-heavy's best p95 over
6 hours, and the best trellis/jvm over 24.

A flat 600 s gives most of the small jobs' gain and loses the big pools': over 24 hours fretboard/jvm's p95 is 31m07 and
trellis/jvm's 30m03, against 20m49 and 20m40, and the worst run with a jvm job 43m29 against 36m38.

The same order holds on either side of 32G over 12 hours:

| Slice | Trigger | All runs | Runs with jvm | light |
|---|---|---|---|---|
| 30G | no barrier | 11m51 / 1m17 | 25m46 / 1h17m | 54s |
| | 120 s | 17m47 / 12m05 | 26m27 / 1h09m | 21m51 |
| | 600 then 120 s | 13m08 / 6m50 | 21m53 / 1h01m | 8m38 |
| 31G | no barrier | 9m56 / 1m13 | 20m04 / 56m08 | 42s |
| | 120 s | 14m02 / 8m34 | 20m15 / 1h00m | 16m30 |
| | 600 then 120 s | 10m08 / 2m33 | 17m56 / 53m38 | 2m21 |
| 33G | no barrier | 7m24 / 1m25 | 12m03 / 35m49 | 40s |
| | 120 s | 8m37 / 3m01 | 12m47 / 45m09 | 4m26 |
| | 600 then 120 s | 7m36 / 1m52 | 11m58 / 30m53 | 1m27 |
| 34G | no barrier | 7m06 / 1m16 | 11m15 / 32m12 | 42s |
| | 120 s | 7m50 / 2m29 | 11m30 / 30m17 | 3m44 |
| | 600 then 120 s | 7m10 / 1m35 | 11m03 / 27m32 | 1m14 |

### Thirteen large jobs

No replayed window has this queue, so a deterministic replay stands in, with **assumed** run times: thirteen 11G jobs queued
at minute 9 in three slots of a 42G slice, and a stream of 7G jobs, one every 80 s for three hours, running 8 minutes each,
six of which fill the slice. Under caps, with passes. The figure is the longest a large job waited, in minutes.

| A large job runs | No barrier | 120 s | 600 s | 600 then 120 s |
|---|---:|---:|---:|---:|
| 2 minutes | 182 | 28 | 36 | 36 |
| 5 minutes | 192 | 24 | 72 | 32 |
| 10 minutes | 212 | 44 | 132 | 52 |
| 20 minutes | 252 | 84 | 92 | 92 |

The 10 minute row is the test `a_pool_of_many_large_jobs_is_not_held_to_one_runner_per_first_limit`, whose first limit costs
8 minutes against 120 s and whose served limit saves 80 against 600 s.

An end hands its room to the next large job, which is older than the stream, so the pool never stalls: the limit decides
how many of its jobs run at once. At 600 s a run no longer than the limit never gets a second one beside it, and the pool runs
one at a time: 12 + 12 × 10 = 132. The served limit gets a second and third up within minutes. At 2 minutes a job ends before
that can happen, so the rule does no better than 600 s, and at 20 minutes 600 s gets there too.

The shape matters too. The stream above is sized so that one end leaves too little. With a stream of 8G jobs in 32G and large
jobs of 2 minutes, one end after a barrier leaves room for the next large job, which starts with no barrier: 600 s and this
rule both give 24 minutes. In that slice with two older 8G jobs before each large job, what they take sets the pace, and the
rule gives 67 minutes against 63 at 600 s and 67 at 120 s.

### Limits

- One night, and the windows nest, so three windows are not three samples.
- No host limit, so 32G stands in for the over-reservation and the programs outside the slice that the live host has. The
  starved jobs of 0096's snapshot are still queued in the backup and are not replayed.
- No warm runners, restarts, reloads, pauses or backoffs.
- A replayed job leaves the queue when it starts, so a replayed queue empties a poll before the daemon's can. The daemon's
  mark lasts up to a poll longer.
- A moved job is known in its new pool from the start, which the daemon was not.
- Rows move a minute or two on no change of policy.

## Consequences

Under contention most short shortages no longer stop younger jobs, and a light job's p95 falls by minutes. In exchange a large
job that is really starved waits 10 minutes where it waited 2 before anything happens. Every restart inside that wait starts
it again, and so does a start that fails for a moment: one 502 from GitHub brings a 30 s backoff, the job then waits another
600 s, and on a served pool the whole queue is back on the first limit. Both cost five times what they did at 120 s. A pool
that started its jobs with no barrier has no mark, so its next starved job waits the 600 s too.

0096's constants, its "2-minute windows" and the replay tables of 0096 and 0097 describe the 120 s limit. Those tables stay
as what was measured then.
