# 0131. A job passes a barrier unless what passed would keep its owner out

Status: **Accepted** (2026-11-10). Amends [0097](0097-a-job-may-start-behind-a-barrier-it-cannot-cost.md): its rule 4
and its second bound. No wire, schema or config change.

## Context

Rule 4 of 0097 counts the profile caps of every runner started since the barrier matured (T0). That includes runners
started for jobs older than the owner. The owner waits those out under a strict barrier too, and 0097's second bound
already names them: "the owner fits once every runner started before T0 has ended, along with the runners of jobs older
than it." Counting them again closes backfill until they end. At 14:22:38 UTC on 2026-11-10 the owner, fretboard's
`check:backend:static` (jvm-heavy, 11G), had matured at 14:22:03. Three runners started after that for older jvm and
node jobs, and GitHub gave them younger jobs of the same pools. Their caps, 13.25 + 9.75 + 9.75 = 32.75G, left what a
drain gives (39.81G, by the host) less the owner short by 3.94G, so a light job of 1.75G cap and 0.5G reservation could
not pass. Barriers stood 92 minutes on 11-09 and 80 on 11-10.

## Decision

Rules 1 to 3 stay. Rule 4 becomes:

4. The profile caps of every runner that **passed a barrier** at or after T0, plus this one's, stay within what a drain
   gives less the owner, for memory, host and CPU. An older job a drain serves counts here if it needs more than the
   owner, as before.

- **Passed**: its plan started it behind a barrier (`Decision::Start::behind`). A runner started for a job older than
  the owner, a warm runner (none starts behind a barrier, 0096) and a runner started while no barrier stood did not
  pass. A runner that passed another barrier since T0 counts: it is younger than that owner, may be younger than this
  one, and counting it only passes less.
- **Caps**, not what it takes: a job that passed may hang and grow to its cap, and rule 4 is what keeps the owner's
  room through that. Counting what passed at its take instead (the larger of its reservation and its use) bought 0.2
  to 0.3% of waiting in the replay below and gave up that guarantee: a job that passed, filled the room, then grew past
  its reservation and hung would keep the owner, and everything behind it, out until the job's timeout.
- A runner that passed and that GitHub gives no job, a twin from listing lag among them, still counts at its caps until
  it goes, at `IDLE_TIMEOUT` or for `Warm::make_room`. For S it counts as ending now, as before.

`priority::Backfill` stays the one rule; `plan` and `admission::simulate` both call it. The replay reads a running job's
`backfilled` mark as passed, so the shared fixture still starts its jobs at the same moments in both.

### Where it is kept

`Decision::Start` already named the owner. The start in flight keeps it (`Starting`, ADR 0125), and the runner's
engagement keeps it from there, written once at the start (`Origin::Started { purpose, behind }`, ADR 0119).
`RunnerSlot::behind` carries it to the plan, and `priority::Runner::passed` to the rule.

- A reload keeps it, with the engagement.
- A restart loses it, with the waits it relates to: an adopted runner did not pass. That changes nothing rule 4 reads.
  A restart loses every wait (0096), so every barrier after it matures at least `PRIORITY_AFTER` after it, and every
  adopted runner started before that T0. Only a clock set back by more than that limit could put an adopted runner after
  T0, and it would then count as not passed.

### What it promises

The first bound, that an owner keeping its barrier starts no later than under a strict one if each run takes what the
plan read, does not rest on rule 4 and stands as 0097 says.

The second bound becomes: **whatever the run times do, the owner fits once every runner started before T0, for a job
older than it, or with no barrier standing has ended.** The runners it now leaves out are ones the owner waits out under
a strict barrier too. What passed keeps leaving the owner its room at its caps, so it holds if a job that passed hangs
or grows to its cap, as 0097's did.

### The proof

- The 14:22:38 status as a plan (`at_14_22_38`): no light job passes on main, one passes now, and the owner starts
  beside it once the older runners end.
- Unit tests that fail under each mutant: counting every start since T0, counting what passed at its take instead of
  its caps, a twin or idle runner that passed not counted, a warm runner counted as passed, and the engine dropping
  `behind` when the runner is placed.
- The generated hang check runs a big job of more than half the slice (`HEAVY_STREAMS`), with jobs that pass, hang and
  grow to their caps once the room has filled. It fails when rule 4 counts what passed at its take, and when it counts
  nothing.
- `every_world_of_six` holds both bounds and the ceiling on every small world: no owner started late on exact run
  times, none past the later of its strict start and the end of what passed it on run times read double, and none was
  kept out by jobs that hang. `SHARED`, up to 5 jobs: 2,163,756 worlds, 105,372 passes. `TIGHT`, up to 6: 26,051,628
  worlds, 6,946,348 passes; 17,432 owners that kept their barrier started late on run times read double, by a tick, and
  140 that lost theirs, by a tick, as under 0097. `TWO_LIMITS`, up to 6: 2,546,560 passes against 0098's 2,291,604;
  4,220 started late on run times read double, as before, and none that lost its barrier now started late, against 80.
  The test asserts that last count.

### Replay

The density study's scratch replay (`replay.py`, not `runnerd simulate`), main's count, rule 4 by caps of every start
against rule 4 by caps of what passed. Waits are arrival to runner start; light is fretboard's.

| Window | Rule 4 | All waits | light p95 | jvm-small p95 | jvm-heavy p95 | Passes | Barrier stood |
|---|---|---:|---:|---:|---:|---:|---:|
| W3, since 0.0.29 (8,102 jobs) | every start | 497.8 job-h | 15m10 | 11m07 | 30m23 | 51 | 225 min |
| | what passed | 451.6 job-h | 13m14 | 6m53 | 31m20 | 169 | 228 min |
| W7, 11-03 to 11-10 (20,939 jobs) | every start | 820.2 job-h | 4m39 | 2m18 | 21m33 | 227 | 349 min |
| | what passed | 873.9 job-h | 5m51 | 3m14 | 21m08 | 361 | 370 min |
| 11-10 alone, from an empty start (3,008 jobs) | every start | 368.0 job-h | 18m01 | 12m09 | 34m55 | 63 | 134 min |
| | what passed | 359.5 job-h | 15m58 | 11m39 | 34m40 | 124 | 136 min |

The host's lowest `MemAvailable` and the runs past their reservation do not move. Counting what passed at its take
instead of its caps gives 450.9 and 871.3 job-hours and the same 11-10.

W3 waits 9% less. W7 waits 7% more, and most of that is 11-10, where the replay depends on how it enters the day. This
rule waits 323.4 job-hours on 11-10 entering from W3 or W7 and 359.5 from an empty start; every start waits 370.4,
267.8 and 368.0. The 267.8 is the outlier, and from the other two entries this rule waits less on 11-10, light p95
included. Outside 11-10, W7 waits 0.3% less (552.4 to 550.5 job-hours), but light's p95 is slightly worse, 3.40 to 3.78
minutes, mostly on 11-05 (2.3 to 5.0). The model errs by more than these differences (the study's finding 8).

## Consequences

Small jobs pass a big one while runners started for older jobs still run. What passed still counts at its caps, so a
small job with a large cap passes less, as 0097 says. A slot's runner keeps one more fact from its start, which a
restart drops.
