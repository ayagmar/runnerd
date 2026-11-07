# 0107. A warm runner waits only where jobs just came

Status: **Accepted** (2026-11-07). Amends [0050](0050-warm-runners.md). No wire, schema or config change.

## Context

The live history from 2026-11-01 to 2026-11-07 (65 hours of uptime; the last 39 with today's profiles) was replayed
under the old rules and the new ones. Under the rules below, misses fall 32% (1,797 to 1,226 in 39 h), room stops
fall 30% (1,624 to 1,143) and hits rise 13% (1,875 to 2,117). Wait times barely move: the mean goes from 189.3 s to
188.4 s and the p95 stays at 1,052 s. Long waits come from memory contention between real jobs, which no warm-runner
rule touches.

Idle memory is a smaller matter. Of the 1,193 GB-h that idle warm and burst runners reserved, 99.2% fell in moments
when no job had waited more than 30 s: they displaced next to nothing, since none starts while a job waits and a job
stops them for room. An idle runner uses about 95 MB and 0.006 cores. The cost of a miss is churn: a container
started, registered and removed, and a room stop on the job's path.

Two rules made most of the misses:

- **The usual hours.** A repository was active during any hour of the day in which its jobs came on 3 of the last 14
  days. trellis and glowly were active for hours with no job. The rule started 420 runners in 65 h that took 4 jobs:
  31% of all miss memory. These pools come first in the config, so the round-robin gave them room and slots ahead of
  fretboard's bursts, and fretboard's jobs then stopped them for room.
- **Bursts sized by the runs that used the pool.** `Learned` took the median over runs that queued at least one job
  for the pool. fretboard `ci` queued no jvm-small job in 374 of 562 runs (39 h), but the median over the other 188 was
  4. So every `ci` run asked for 4 jvm-small runners, including the 240 that queue only `gate`.

## Decision

- **A repository is active for `ACTIVE_AFTER` = 10 minutes past the last tick that listed one of its jobs queued, the
  last take of one of its jobs by a runner, or the last finish of one of its jobs.** The usual-hours rule goes. When a
  runner that ran a job ends, its repository is noted active from then, so a push a few minutes after a long job
  finds a runner up. On a restart, and at each read of the history, the history seeds activity from each job's
  `queued_at` and `finished_at`; notes taken since the last read are kept (`Learned::absorb`). Outside that window a
  pool's configured warm runners go once they have idled `IDLE_TIMEOUT`, as before.
- **A workflow's jobs per run for a pool is the lower median over every attempt of that workflow in the repository
  seen in `LOOKBACK`, an attempt with no job for the pool counting 0, less the run's jobs seen for the pool.** An
  attempt counts when the history has a job of it that one of the repository's pools serves. A pool still needs
  `MIN_RUNS` (3) attempts with jobs for it before it gets a burst. The span (p90 from the attempt's first job, over
  those attempts) is computed as before.
- **A reload has the history read again at the next tick.** The read is due at once (its time is set back); the
  `Learned` is kept, so `absorb` keeps the notes taken since. A reload that moves labels to another pool, as the
  jvm-small split did, then moves their bursts with them, rather than keeping the old pools' for up to `REREAD`.
- On by default. No config key, no schema or wire change: `finished_at` is an existing column. `warm = 0` still turns
  both rules off.

## Consequences

Replayed over the same history (`warm-study-replay/replay.py`, the rule as built: listings, takes and finishes note
activity at each 15 s tick):

| | 39 h, old | 39 h, 0107 | 65 h, old | 65 h, 0107 |
|---|---|---|---|---|
| hits | 1,875 | 2,117 | 2,333 | 2,712 |
| misses | 1,797 | 1,226 | 2,296 | 1,637 |
| warm and burst starts | 3,672 | 3,343 | 4,629 | 4,349 |
| room stops | 1,624 | 1,143 | 2,054 | 1,461 |
| miss runner-hours | 128.8 | 40.6 | 170.3 | 58.2 |
| miss GB-h | 585 | 221 | 867 | 347 |
| wait mean / p95, s | 189.3 / 1,052 | 188.4 / 1,052 | 281.0 / 1,778 | 280.2 / 1,773 |

The replay is optimistic in absolute terms: it replays 1,875 hits where the history has 1,513. Read the gains as
relative.

- **A pool used in fewer than half of a workflow's runs gets no burst from it.** The median flips at 50%. Today that
  is fretboard `ci`'s jvm, node, jvm-small and jvm-heavy, used in a third to two fifths of its runs. jvm-small goes
  from 257 hits to 87 in 39 h, with no burst starts. Each lost hit costs about 32 s, about 1.5 job-hours per 39 h.
  fretboard jvm goes from 32 hits to 94, node from 121 to 206, light from 1,434 to 1,708: workflows that always use a
  pool (`design` for node, `images` for jvm and node) keep their bursts, and the room the old bursts took goes to
  them. A one-third quantile gains 1 hit in 39 h for 0.5 to 2 more runner-hours, not worth a parameter.
- **A quiet repository's first job of a session starts cold.** trellis and glowly had 28 hits in 39 h under the old
  rules and 21 under these, nearly all of the lost ones daily scheduled jobs nobody waits on. A repository whose jobs
  come every 15 to 60 minutes keeps no configured runner between them.
- **A restart in the middle of a run can start more burst runners than the run needed,** as before: `Runs` comes back
  from the open jobs only, so the run's finished jobs are no longer seen. The bound is the run's burst less its open
  jobs; with fretboard `ci`'s light median of 12, up to 12 less the open light jobs. The generated engine sequences
  assert that bound.
- **A reload that moves labels while a run is open over-bursts the new pool** by the run's jobs seen under the old
  one, at most its burst there, until the run stops counting (its span plus `IDLE_TIMEOUT`).
- Nothing is stored, so the previous release reads the same history.
- Rejected:
  - Per-pool hit-rate feedback: 4 more hits and 2 fewer runner-hours, for state that would have to survive restarts.
  - Keying bursts by the run's first job name: 317 fewer hits in 39 h, as `gate`-only runs' bursts hit other runs'
    jobs.
  - Counting an idle warm runner at a smaller share: at most 0.3 to 2.2 job-hours per 39 h, and GitHub may hand it
    any job of its pool, so the room that job needs must already be there.
  - A shorter idle time for bursts: bursts end by room or by taking a job; 96 expired in 6 days.
  - Bursts before configured warm runners: 1 more hit.
