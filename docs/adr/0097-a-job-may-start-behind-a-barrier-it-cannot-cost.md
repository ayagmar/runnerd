# 0097. A job may start behind a barrier it cannot cost

Status: **Accepted** (2026-11-06), amended by [0098](0098-a-pool-waits-longer-for-its-first-barrier.md), whose limits
move every replay figure here, and by [0099](0099-the-budget-counts-jobs-that-do-not-peak-at-once.md), which counts the
largest credit when it says when room gathers. Amends [0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md). Passes follow
[0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md). No wire, schema or config change.

## Context

Under 0096 a barrier holds every younger job until its owner's runner fits. While it stands, memory sits free that a
short job could use: on the owner's history a strict barrier cost `light` a p95 of over a minute and a maximum of 12
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
   owner, for memory, host and CPU. An older job a drain serves counts here if it needs more than the owner: when it
   matures it takes the barrier.

A job that passes starts with no reason. One that does not says what it says under 0096. It is on by default, under both
admission modes, with no config key. `priority::Backfill` is the one rule; `plan` and `admission::simulate` both call it,
and a refill pass goes through `plan`, so it obeys the same rule. A plan works it out from what it was given and keeps
nothing, so a reload that changes R recomputes it at the next plan and a restart leaves nothing behind. The interlocks
of 0096 take no part: while one holds, no barrier stands and nothing starts.

Older jobs are in rule 2 because they go first. If a younger job takes the room such a job was about to use, that job
starts late, ends late, and the owner, whose own room came on time, starts late. A job older than the owner waiting for
room or for a slot therefore counts from when the running runners let it start. One in a preparing profile counts by
its room alone, as preparation ends on its own. One that no running runner's end serves does not count, so it cannot
stop every job behind the barrier.

A pool that is paused or backing off never reaches the plan's job list, so its oldest job is no older job here. It goes
by what is free when it comes back, and a pass may have taken that.

A runner started in this plan has no job yet. Reading it as ending at its pool's longest run time would put S too late,
since GitHub may give it a shorter job, so it counts as ending now until a later plan knows its job.

The slot in rule 1 keeps the second bound true: a job that hangs in the owner's last free slot would keep it out however
much memory is free.

### What it promises

Up to S, this world and a strict one differ only by jobs that have ended, given that no start before S needs their room.
That argument is the first bound, and each of its assumptions is a way it fails:

- S is early enough: the runners end when the plan read they would.
- Every job behind the barrier ends by S: each takes no longer than what the plan read, including a job GitHub picks
  and one queued after the plan.
- No start before S needs the room: rule 2 sees the older jobs, except those of the pools above.
- R and what a drain gives stay put. The host limit is the one that moves in practice, so the promise is weakest under a
  short host, which the replay cannot test.

**If each run takes what the plan read, no less and no more, an owner that keeps the barrier starts no later than under
a strict barrier.**

Runs do not. The plan reads p95s, so S is a late estimate of the room, and nearly every run ends before its p95: trellis's
quality gate has a p50 of 2 minutes and a p95 of 13. If a runner ends early, the room comes before S, and a job behind
the barrier may still be running. **A run that ends before its estimate can cost the owner up to what is still running
behind the barrier.** Reading p50s for the running runners would end that lateness, but it cut passes from 488 to 106 and
gave back the gain (all runs 13m38 against 10m50 at 32G over 12 hours, with the pools as configured now, measured before
rule 4 counted older jobs), so the rule stays and the promise is this one.

**Whatever the run times do, the owner fits once every runner started before T0 has ended**, along with the runners of
jobs older than it. Rule 4 leaves what those runners could not take, at their caps, for the owner. It holds if a job
behind the barrier hangs, or grows to its cap, as the rule counts caps and not what runners take now.

So a job that overruns, or one that runs on after an early end, can delay the owner, up to the second bound. Rule 3 keeps
that delay small in practice; only rule 4 limits it.

Neither bound composes across barriers. A pass kept the bounds of the barrier it passed, but it changes who runs when, so
it can delay a later owner, including one whose barrier an older job took when it matured. In one generated world, under
0096's single limit, job 102 owned a barrier at tick 90 and lost it to older jobs. Jobs passed behind the older barrier, an older job started a
tick late, and 102 started a tick late. The next job of a pool waits out its own limit, and a hung runner started before
its T0 counts as running, until the job's own timeout ends it.

A backfilled runner that GitHub gives no job counts as ending now, and stays in its slot and on the budget up to
`IDLE_TIMEOUT` unless `Warm::make_room` stops it. The first bound does not cover that wait.

How late owners really were, measured by forking one state at each owner's first pass and going on with and without
passes. All three rows are from before rule 4 counted older jobs and before 0098, and the fork is not in the tree:

| World | Owners | Late | Worst |
|---|---|---|---|
| Generated runs, estimates read double, 128 seeds | 1,807 | 35 | 9 ticks (4m30) |
| 32G 12h replay | 32 | 1 | 12 s |
| 32G 12h replay, pools as configured now | 42 | 6 | 77 s |

Tests check these on every small world instead of a sample of large ones: 3 pools, durations 1 to 4 ticks, 2 slots a
pool, a job starved after one tick, and every pattern of arrivals 0 or 1 tick apart among the first four jobs. That is
26,051,628 worlds of up to 6 jobs with memory and CPU both short, and 2,163,756 of up to 5 jobs with two pools sharing a
profile.

- A run takes what the plan read: no owner started late, whether it kept its barrier or not.
- Estimates read double: no owner that kept its barrier started past the later of its strict start and the last end of
  what passed it. 140 that lost theirs started late, by a tick.
- Jobs behind a barrier hang: no owner was kept out, unless an older job took its barrier.

These worlds have no warm runners, preparation, reloads or host limit. Up to 3 jobs run in every `cargo test`; the rest
take about 11 minutes with 0098's third shape (`every_world_of_six`, ignored).

They are no evidence for the older jobs of rule 2: with that clause removed every count above is the same, up to 6 jobs. What
fails without it is its unit tests and the replay and planner's shared fixture.

### Usual run times

`admission::Predictor::learn` keeps, with each job's peaks, how long its last 20 runs kept their runners. A job's usual
run time is the p95 of the successful ones, from 3 of them. Cancelled and failed runs teach nothing: they end early or at
a timeout, and a short estimate is the expensive mistake here. The time counts from when the runner started, or the job
queued if later, to the end, so it includes the container's start (about 6 seconds in the owner's history, up to two
minutes). It errs long. In rule 3 that passes less; in rule 2 it puts S late and passes more, which is the cost above.

The engine reads the predictor under both admissions. `Repository::usual` is U(P), `RunnerSlot::usual` the running job's.
U(P) reads the listing past the jobs runners are known to have taken: a job hidden for a runner no capture named yet may
still be queued.

Under caps admission no reservation or sizing changes, as every use of the predictor for memory or CPU is filtered by
policy. Occupied slots get their job's key under caps too, so `vacate` keeps more slot affinity, which is harmless. It
costs one history read a minute, as measured admission does.

A warm runner's job counts from when it queued in the history, and from the container's start in `left_to_run`, so it
reads as overdue and counts as ending now. That is the safe side and costs passes while a warm runner's room is what the
owner needs. Counting from the same moment would need the job's queue time on the slot, which the path where GitHub names
the job does not have.

A start behind a barrier logs one `info!` line naming the owner, which is how a sandbox run counts passes.

### Replay

`simulate --priority` now replays no barrier, a strict one and this rule, reports whole runs (first arrival to last end)
and how often a backfilled job passed its usual run time. A job that queued after another of its run ended arrives the
same gap after that job's replayed end, so a held job also holds back what follows it, and it queues behind what arrived
before it, as IDs follow queue time in the daemon. The history has no `needs`, so this is read from the times. Each replay knows the 30 days before its window, as the daemon would, and learns as jobs
finish, so these are online estimates and not hindsight.

The owner's history to 2026-11-06, configured slots with `jvm=6`, 32 CPUs. "Cold memory" keeps those run times but
starts memory predictions empty, so jobs reserve their caps at first: this matches how the 0096 replays ran, and took a
patch that is not in the tree. Figures are all runs' mean / p50, runs with a jvm job mean / max, light's p95 / max, and
the p95 of fretboard/jvm and trellis/jvm.

| Slice | Window | Policy | All runs | Runs with jvm | light | fretboard/jvm | trellis/jvm |
|---|---|---|---|---|---|---|---|
| 46G | 6h | none | 5m46 / 54s | 8m00 / 24m29 | 23s / 42s | 6m04 | 5m28 |
| | | strict | 5m47 / 54s | 8m00 / 24m29 | 23s / 2m02 | 5m59 | 5m28 |
| | | backfill | 5m46 / 54s | 8m00 / 24m29 | 23s / 42s | 5m59 | 5m28 |
| | 12h | none | 5m48 / 1m04 | 7m45 / 24m38 | 25s / 50s | 5m55 | 4m59 |
| | | strict | 5m48 / 1m04 | 7m45 / 24m38 | 25s / 2m02 | 5m56 | 4m59 |
| | | backfill | 5m48 / 1m04 | 7m45 / 24m38 | 25s / 50s | 5m56 | 4m59 |
| | 24h | none | 5m45 / 1m04 | 7m44 / 24m38 | 25s / 42s | 5m55 | 4m59 |
| | | strict | 5m46 / 1m04 | 7m44 / 24m38 | 25s / 2m02 | 5m56 | 4m59 |
| | | backfill | 5m45 / 1m04 | 7m44 / 24m38 | 25s / 42s | 5m56 | 4m59 |
| 46G, cold memory | 6h | none | 6m34 / 57s | 10m32 / 30m57 | 26s / 59s | 15m31 | 17m38 |
| | | strict | 7m15 / 1m42 | 10m54 / 32m28 | 1m09 / 12m17 | 15m25 | 16m58 |
| | | backfill | 6m52 / 1m10 | 10m37 / 32m52 | 49s / 11m11 | 13m51 | 16m58 |
| | 12h | none | 6m25 / 1m07 | 9m16 / 24m57 | 25s / 1m20 | 10m08 | 11m24 |
| | | strict | 6m33 / 1m33 | 9m17 / 24m57 | 29s / 8m25 | 8m54 | 10m34 |
| | | backfill | 6m29 / 1m15 | 9m13 / 24m57 | 29s / 8m25 | 9m02 | 10m45 |
| | 24h | none | 6m15 / 1m07 | 9m00 / 24m57 | 25s / 1m20 | 8m36 | 10m15 |
| | | strict | 6m15 / 1m10 | 8m50 / 24m57 | 26s / 5m34 | 7m30 | 9m10 |
| | | backfill | 6m14 / 1m09 | 8m51 / 24m57 | 25s / 6m02 | 7m40 | 9m32 |
| 32G | 6h | none | 9m35 / 1m02 | 20m05 / 52m51 | 32s / 2m56 | 41m42 | 38m47 |
| | | strict | 11m41 / 3m16 | 21m23 / 58m31 | 8m31 / 24m52 | 32m38 | 33m52 |
| | | backfill | 10m54 / 2m11 | 20m47 / 55m27 | 4m46 / 21m13 | 33m10 | 32m39 |
| | 12h | none | 11m22 / 1m13 | 24m29 / 55m45 | 33s / 1m48 | 46m03 | 40m14 |
| | | strict | 15m47 / 9m29 | 26m45 / 1h05m | 16m51 / 30m25 | 34m42 | 29m06 |
| | | backfill | 13m23 / 5m10 | 25m22 / 56m22 | 6m11 / 23m51 | 38m27 | 39m33 |
| | 24h | none | 11m53 / 1m16 | 26m22 / 1h02m | 29s / 3m42 | 47m46 | 42m21 |
| | | strict | 16m18 / 8m48 | 28m35 / 1h07m | 12m01 / 31m47 | 38m36 | 39m19 |
| | | backfill | 13m06 / 4m27 | 24m48 / 57m03 | 7m42 / 23m27 | 38m38 | 37m39 |

A backfilled job passed the usual run time it was let through by rarely, and by little:

| Slice | Window | Started behind a barrier | Overran | By at most |
|---|---|---|---|---|
| 46G | 6h / 12h / 24h | 11 / 11 / 11 | 1 / 1 / 1 | 2s |
| 46G, cold memory | 6h / 12h / 24h | 84 / 55 / 43 | 2 / 1 / 1 | 6s / 2s / 2s |
| 32G | 6h / 12h / 24h | 197 / 485 / 466 | 3 / 10 / 9 | 13s / 23s / 23s |

That measures against U(P), not the job's own estimate, and its run time leaves out the container's start, so it reads
low. It says nothing of how far S was off, which is the risk the table of late owners shows.

With measured memory known from the start, 46G holds this load: barriers stand under 3 minutes in a window and the three
policies nearly match, which also says much of the contention in 0096's replay was a cold start at caps.

Under contention, backfill beats strict on all runs' mean and p50 in every window at 32G, by 47 seconds to over 3 minutes
on the mean, and on runs with a jvm job, mean and max. Over 12 and 24 hours the worst such run is close to no barrier's
or better (56m22 against 55m45, 57m03 against 1h02m). With cold memory it is ahead of strict by 23 seconds over 6 hours
and by a few seconds past that, as barriers stand less. The same order holds at 30G, 31G, 33G and 34G over 12 hours (all runs 27m09, 20m34,
10m07 and 9m03 against strict's 32m17, 25m54, 10m49 and 9m14).

Light gains most: at 32G its p95 falls from 8 to 17 minutes under strict to 5 to 8. The big pools can pay for that.
fretboard/jvm's p95 matches strict's over 6 and 24 hours and gives back 3m45 over 12, and trellis/jvm's gives back nearly all
of strict's gain over 12 hours (39m33 against 29m06). No policy here is best for every pool.

Limits of these figures:

- One night: the windows nest, and 12 and 24 hours differ by 5 runs of 496. Rows move a minute or two on no change of
  policy.
- No host limit, so less contention than the daemon sees. 46G does not reproduce that night: at 02:45 reservations stood
  at 42.8G against 24.0G used, with a host limit of 44.5G. So 32G stands in for over-reservation as much as for what
  ran outside the slice.
- No warm runners or restarts.
- A job keeps the runner started for it, where GitHub may give that runner any job of its pool, so the replay knows a
  fresh runner's job one plan before the daemon does.
- Dependencies are read from timing.
- The 97 jobs queued or running when the history ends have no run time and are left out.
- The heavy jobs ran in `jvm`, so jvm-heavy has one replayed job.
- Memory is modelled at each job's peak. Run time estimates are the daemon's own.

## Consequences

Small jobs can run while a large job waits, wherever the estimates allow. The price is that owners can start later than
under a strict barrier, usually by seconds, by what still runs behind them when their room comes early, or up to the
second bound when a job overruns. The planner now depends on history it did not read before, and under
caps admission the predictor is read for run times alone.

A pool with one job that has no estimate gets the strict barrier, so new jobs wait until they have 3 successful runs.
Memory and CPU are not predicted here: rule 4 stands on caps, and a large cap on a small job makes it pass less.
