# 0036. A young job predicted from its own runs, one copy at a time

Amends [0033](0033-measured-admission.md), which predicted a job with fewer than `[sizing] min_jobs` runs as the
larger of its profile's p95 and its own largest peak.

## Context

A profile often mixes light and heavy jobs. On one host's first day under measured admission, a node profile ran
200M checks, 2G image builds and a 4.8G stack: its p95 was about 4.7G, so every job with fewer than 10 runs reserved
5.5G, even one whose 6 runs all peaked at 1.3G. Five runners filled a 33G slice that used about 8G, and jobs waited
15 minutes. Setting `min_jobs = 3` cut the waits, but `min_jobs` also gates the profile: with 3 light runs of the
profile, a new 2G image build was predicted at 512M.

A few runs of a job can also all be light for a reason that does not last: a warm cache, or a failure before the
heaviest step. After a cache is cleared, every copy of that job may queue at once and need the heavy amount. The
samples cannot hold them back in time: each copy uses little while it warms up, so the slice looks empty while
they start.

## Decision

A job with fewer than `min_jobs` runs, in a profile with `min_jobs` runs, reserves its own largest peak plus the
margin (`Basis::Job` with `peak: largest`) when:

- at least 2 of its runs (`admission::OWN_RUNS`) completed with success; failed and lost runs still count for its
  largest peak, not for this;
- and no other copy of it is queued or running (`admission::Copies`). Copies queued together, and a copy queued
  while another runs, take the old rule. Of copies running at once, the first in slot order goes by its own runs
  once its job is known, and the others by the old rule.

Otherwise the old rule holds: the larger of the job's largest peak and the profile's p95, plus the margin. Below
`min_jobs`, the p95 of a job's runs is their largest anyway (nearest rank, under 20 runs). The rules for no runs,
runs at the cap, runs past what they reserved and the high-water mark are unchanged, as is the default
`min_jobs = 10`.

So at most one copy of a young job reserves less than the old rule would. The others reserve as much as before,
and whatever that one copy goes past its prediction is bounded by its cap and counted against the next starts from
its next sample.

On the wire, the new case is the `job` basis with an added `peak` field, written only when it is `largest`. A
reader released before this ignores the field and shows the reservation as "the p95 peak of N runs".

`runnerd simulate` on that host's history (140 jobs over 22 hours, `jvm` 2 and `node` 6 slots). Measured
admission's p95 wait; jobs that peaked above what they reserved; and by how much, added up:

| MemoryMax | Caps | Before, `min_jobs` 10 | Before, `min_jobs` 3 | Now, `min_jobs` 10 |
|---|---|---|---|---|
| 33G | 4m51s | 3m50s; 2, 2.9G | 3m29s; 7, 11.3G | 3m29s; 2, 2.9G |
| 20G | 11m02s | 9m36s; 2, 4.5G | 7m37s; 6, 10.8G | 6m56s; 2, 4.5G |
| 14G | 23m43s | 22m56s; 2, 3.0G | 15m12s; 6, 10.8G | 13m50s; 2, 2.7G |

The replay holds each running job at its peak from its start, so it cannot show the warm-up burst. An engine test
does: two 200M runs of a job, then four copies queued at once that each need 3G after warming up at 300M, in a 10G
slice. The copies start two at a time at 3.5G, and a third once the first copy's job is known and it reserves
256M. Three copies at 3G fit the slice; four, as two light runs alone would admit, do not.

## Consequences

Young jobs reserve what they used, one copy at a time. A burst of copies of a young job starts at the old rule's
pace. A job whose runs keep failing stays on the old rule until `min_jobs` runs. A single copy can still go past
its own runs, by up to its cap; that run makes its job reserve its largest peak for 7 days (0033). In the replay,
the same jobs went above what they reserved under the old rule and this one, by the same amounts except one, which
was 0.3G less.
