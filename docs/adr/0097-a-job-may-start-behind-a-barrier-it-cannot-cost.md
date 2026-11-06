# 0097. A job may start behind a barrier it cannot cost

Status: **Accepted** (2026-11-06). Amends [0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md). Passes follow
[0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md). No wire, schema or config change.

## Context

Under 0096 a barrier holds every younger job until its owner's runner fits. While it stands, memory sits free that a
short job could use: on the owner's history a strict barrier cost `light` a p95 of up to a minute and a maximum of 17
minutes, for jobs that run 10 seconds, and each held job also held back what follows it in its run. The room stays free
because the owner needs more than is left, and the runners that will give it the rest are still running. A job that
ends before they do uses room that was idle anyway. Knowing that takes run times, which the planner did not have.

## Decision

The barrier stays. Behind one whose owner needs memory R and CPU C, and whose wait matured at T0, a younger job of pool P
starts if all of these hold:

1. It has a free slot and the budgets left fit it: slice, host and CPU. In the owner's profile, it leaves a free slot
   for the owner.
2. S exists and is in the future. S is the earliest time the running runners' usual ends let the owner fit, or let a
   job older than the owner start if that comes sooner. Runners give back what they take, soonest end first. A warm or
   idle runner, one started in this same plan, one with no estimate and one past its end all count as ending now, which
   only makes S earlier.
3. now + U(P) <= S. U(P) is the largest usual run time among P's queued jobs, since GitHub may give its runner any of
   them. If any has no estimate, nothing of P passes.
4. The profile caps of every runner started at or after T0, plus this one's, stay within what a drain gives less the
   owner, for memory, host and CPU.

A job that passes starts with no reason. One that does not says what it says under 0096. It is on by default, under both
admission modes, with no config key. `priority::Backfill` is the one rule; `plan` and `admission::simulate` both call it,
and a refill pass goes through `plan`, so it obeys the same rule. A plan works it out from what it was given and keeps
nothing, so a reload that changes R recomputes it at the next plan and a restart leaves nothing behind. The interlocks
of 0096 take no part: while one holds, no barrier stands and nothing starts.

Older jobs are in rule 2 because they go first. If a younger job takes the room such a job was about to use, that job
starts late, ends late, and the owner, whose own room came on time, starts late. A job older than the owner waiting for
room or for a slot therefore counts from when the running runners let it start. One that no running runner's end
serves does not count, so it cannot stop every job behind the barrier.

A runner started in this plan has no job yet. Reading it as ending at its pool's longest run time would put S too late,
since GitHub may give it a shorter job, so it counts as ending now until a later plan knows its job.

The slot in rule 1 keeps the second bound true: a job that hangs in the owner's last free slot would keep it out however
much memory is free.

### What it promises

Two bounds, both for the owner of the barrier as it stands, and both only while R and what a drain gives stay put.

**With usual run times that hold, the owner starts no later than under a strict barrier.** Everything started behind it
has ended by S, before the owner or any job older than it could use the room, so they see the world they would have
seen. "Hold" means each job, including a runner's job that GitHub picks and a job queued after the plan, ends within what
the plan read.

**Whatever the run times do, the owner fits once every runner started before T0 has ended**, along with the runners of
jobs older than it. Rule 4 leaves what those runners could not take, at their caps, for the owner. It holds if a job
behind the barrier hangs, or grows to its cap, as the rule counts caps and not what runners take now.

Between the two, a job that overruns can delay the owner, up to the second bound. Rule 3 keeps that delay small in
practice; only rule 4 limits it.

Neither bound outlives its barrier. The next job of the pool waits out its own limit, and a hung runner started before
its T0 counts as running, until the job's own timeout ends it.

A backfilled runner that GitHub gives no job counts as ending now, and stays in its slot and on the budget up to
`IDLE_TIMEOUT` unless `Warm::make_room` stops it. The first bound does not cover that wait.

### Usual run times

`admission::Predictor::learn` keeps, with each job's peaks, how long its last 20 runs kept their runners. A job's usual
run time is the p95 of the successful ones, from 3 of them. Cancelled and failed runs teach nothing: they end early or at
a timeout, and a short estimate is the expensive mistake here. The time counts from when the runner started, or the job
queued if later, to the end, so it includes the container's start (about 6 seconds in the owner's history, up to two
minutes). It errs long, which makes rule 3 pass less.

The engine reads the predictor under both admissions. `Repository::usual` is U(P), `RunnerSlot::usual` the running job's.

### Replay

`simulate --priority` now replays no barrier, a strict one and this rule, reports whole runs (first arrival to last end)
and how often a backfilled job passed its usual run time. A job that queued after another of its run ended arrives the
same gap after that job's replayed end, so a held job also holds back what follows it. The history has no `needs`, so
this is read from the times. Each replay knows the 30 days before its window, as the daemon would, and learns as jobs
finish, so these are online estimates and not hindsight.

The owner's history to 2026-11-06, configured slots with `jvm=6`, 32 CPUs. "Cold memory" keeps those run times but
starts memory predictions empty, so jobs reserve their caps at first: this matches how the 0096 replays ran, and took a
patch that is not in the tree. Figures are all runs' mean / p50, runs with a jvm job mean / max, light's p95 / max, and
the p95 of fretboard/jvm and trellis/jvm.

| Slice | Window | Policy | All runs | Runs with jvm | light | fretboard/jvm | trellis/jvm |
|---|---|---|---|---|---|---|---|
| 46G | 6h | none | 5m50 / 54s | 8m14 / 24m28 | 23s / 1m09 | 6m09 | 5m02 |
| | | strict | 5m50 / 54s | 8m14 / 24m28 | 24s / 2m02 | 6m09 | 5m02 |
| | | backfill | 5m50 / 54s | 8m14 / 24m28 | 23s / 1m09 | 6m09 | 5m02 |
| | 12h | none | 5m50 / 1m04 | 7m55 / 24m28 | 26s / 1m08 | 6m02 | 4m39 |
| | | strict | 5m50 / 1m04 | 7m54 / 24m28 | 27s / 2m02 | 6m02 | 4m00 |
| | | backfill | 5m50 / 1m04 | 7m54 / 24m28 | 26s / 1m08 | 6m02 | 4m00 |
| | 24h | none | 5m48 / 1m04 | 7m53 / 24m28 | 26s / 1m08 | 6m02 | 4m39 |
| | | strict | 5m48 / 1m04 | 7m53 / 24m28 | 26s / 2m02 | 6m02 | 4m00 |
| | | backfill | 5m47 / 1m04 | 7m53 / 24m28 | 26s / 1m08 | 6m02 | 4m00 |
| 46G, cold memory | 6h | none | 6m37 / 56s | 10m39 / 32m32 | 27s / 1m36 | 15m40 | 16m59 |
| | | strict | 7m08 / 1m19 | 10m46 / 34m00 | 1m00 / 16m59 | 12m19 | 17m53 |
| | | backfill | 6m50 / 1m01 | 10m49 / 32m33 | 37s / 17m06 | 12m18 | 16m59 |
| | 12h | none | 6m52 / 1m07 | 10m39 / 28m06 | 29s / 1m20 | 16m00 | 17m45 |
| | | strict | 7m07 / 1m46 | 10m26 / 28m04 | 42s / 15m18 | 12m19 | 15m30 |
| | | backfill | 7m06 / 1m42 | 10m33 / 28m04 | 59s / 15m18 | 13m24 | 15m30 |
| | 24h | none | 6m29 / 1m06 | 9m43 / 24m28 | 29s / 1m28 | 12m58 | 13m41 |
| | | strict | 6m43 / 1m41 | 9m40 / 25m27 | 37s / 12m41 | 10m42 | 12m45 |
| | | backfill | 6m38 / 1m19 | 9m39 / 25m27 | 41s / 12m41 | 12m41 | 12m45 |
| 32G | 6h | none | 9m38 / 59s | 20m27 / 53m19 | 33s / 1m28 | 41m50 | 39m50 |
| | | strict | 11m32 / 3m44 | 21m40 / 1h03m | 8m06 / 44m00 | 32m23 | 26m51 |
| | | backfill | 10m40 / 2m16 | 20m27 / 57m51 | 4m20 / 27m47 | 32m39 | 26m53 |
| | 12h | none | 11m30 / 1m09 | 25m02 / 56m13 | 42s / 1m54 | 46m49 | 39m35 |
| | | strict | 15m54 / 8m47 | 28m13 / 1h28m | 9m46 / 49m26 | 34m02 | 38m01 |
| | | backfill | 17m09 / 10m37 | 31m46 / 1h29m | 13m40 / 1h10m | 39m19 | 38m27 |
| | 24h | none | 11m53 / 1m09 | 26m28 / 57m35 | 39s / 1m54 | 48m06 | 40m41 |
| | | strict | 14m27 / 7m48 | 25m37 / 1h13m | 9m07 / 43m04 | 33m14 | 36m17 |
| | | backfill | 13m26 / 6m04 | 24m34 / 1h12m | 6m58 / 38m28 | 35m59 | 34m47 |

A backfilled job passed its usual run time rarely, by a little:

| Slice | Window | Started behind a barrier | Overran | By at most |
|---|---|---|---|---|
| 46G | 6h / 12h / 24h | 12 / 11 / 11 | 1 / 1 / 1 | 2s |
| 46G, cold memory | 6h / 12h / 24h | 82 / 59 / 45 | 1 / 1 / 1 | 2s |
| 32G | 6h / 12h / 24h | 175 / 407 / 352 | 3 / 7 / 8 | 8s / 25s / 1m15 |

With measured memory known from the start, 46G holds this load: barriers stand under 3 minutes in a window and the three
policies nearly match, which also says much of the contention in 0096's replay was a cold start at caps.

Under contention, all runs' mean and p50 come back part of the way towards no barrier in five rows of six, by a second
in one. Light's p95 follows at 6 hours and at 32G over 24, and is worse with cold memory over 12 and 24 hours (59s
against 42s, 41s against 37s). fretboard/jvm's p95 gives back part of strict's gain in five rows of six, up to 5 minutes.

It is not uniformly better. At 32G over 12 hours it is worse than strict in every column but trellis/jvm's, which it
nearly matches. Passes change which job starts when, so later barriers go up differently, and nearby slices don't repeat
it: over the same 12 hours, all runs' mean is 10m11 against strict's 10m50 at 33G, and 9m06 against 9m16 at 34G. Whole
histories compared like this show no bound. The tests fork one world at each barrier instead, which is what the first
bound says.

Limits of these figures: no host limit, so less contention than the daemon sees; no warm runners or restarts; a job
keeps the runner started for it, where GitHub may give that runner any job of its pool, so the replay knows a fresh
runner's job one plan before the daemon does; dependencies are read from timing; the 97 jobs queued or running when the
history ends have no run time and are left out; the heavy jobs ran in `jvm`, so jvm-heavy is barely exercised. Run time
estimates are the daemon's own, but memory is modelled at each job's peak.

## Consequences

Small jobs can run while a large job waits, wherever the estimates allow. The price is the first bound's condition: an
overrun can delay an owner, up to the second bound. The planner now depends on history it did not read before, and under
caps admission the predictor is read for run times alone.

A pool with one job that has no estimate gets the strict barrier, so new jobs wait until they have 3 successful runs.
Memory and CPU are not predicted here: rule 4 stands on caps, and a large cap on a small job makes it pass less.
