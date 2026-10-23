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
- and no other copy of it is queued or may be running (`admission::Copies`). Copies queued together, and a copy
  queued while another runs, take the old rule. A busy runner of the same repository whose job is not known yet,
  because its log capture has not reported it or failed, may run a copy: while there is one, every job of that
  repository takes the old rule, running ones included. Of copies running at once, all known, the first in slot
  order goes by its own runs and the others by the old rule.

Otherwise the old rule holds: the larger of the job's largest peak and the profile's p95, plus the margin. Below
`min_jobs`, the p95 of a job's runs is their largest anyway (nearest rank, under 20 runs). The rules for no runs,
runs at the cap, runs past what they reserved and the high-water mark are unchanged, as is the default
`min_jobs = 10`.

This is not a guarantee that at most one copy reserves less than the old rule would. Between two ticks GitHub may
give a waiting runner a copy, as 0033 notes, and a second copy may queue before that runner shows busy, so two
runners can start on the own-runs prediction. A busy runner whose job is unknown is raised for a heavier job only
while one is queued; with the queue empty, both keep what they reserved until their jobs are known, counted at
what they use from their next samples. Whatever a copy goes past its prediction is bounded by its cap. The rule
narrows the risk of a burst; it does not rule out the slice filling.

On the wire, the new case is the `job` basis with an added `peak` field, written only when it is `largest`. A
reader released before this ignores the field and shows the reservation as "the p95 peak of N runs".

`runnerd simulate` follows the rule as the daemon does each tick: at every moment it predicts the running jobs
again, the first running copy of a job, in the order they started, going by its own runs. It knows each job from
its start, so it never meets a busy runner whose job is not known, and start order stands for slot order.

On that host's history (175 jobs over 22.5 hours, `jvm` 2 and `node` 6 slots), measured admission's p95 wait;
jobs that peaked above what they reserved when they ended; and by how much, added up:

| MemoryMax | Caps | Before, `min_jobs` 10 | Before, `min_jobs` 3 | Now, `min_jobs` 10 | Now, `min_jobs` 3 |
|---|---|---|---|---|---|
| 33G | 4m32s | 3m40s; 2, 2.9G | 2m59s; 7, 11.3G | 2m59s; 2, 3.0G | 2m59s; 4, 7.2G |
| 20G | 10m11s | 9m10s; 2, 4.5G | 6m50s; 6, 10.8G | 6m21s; 2, 3.0G | 6m14s; 4, 7.2G |
| 14G | 30m40s | 23m29s; 2, 3.0G | 13m27s; 6, 10.8G | 11m52s; 2, 2.7G | 11m33s; 4, 7.2G |

The replay holds each running job at its peak from its start, so it cannot show the warm-up burst. Two engine
tests do, on a 10G slice where a job's two runs peaked at 200M and its profile's p95 is 3G. In one, four copies
queue at once and warm up at 300M: they start two at a time at 3.5G, and a third once the first copy's job is
known and it reserves 256M. Three copies at 3G fit the slice; four, as the two light runs alone would admit, do
not. In the other, the copies arrive one per poll and no log capture reports which job a runner took: only the
first starts at 256M, and once a second copy queues beside the busy runner both reserve 3.5G.

## Consequences

Young jobs reserve what they used, one copy at a time. A burst of copies of a young job starts at the old rule's
pace. A job whose runs keep failing stays on the old rule until `min_jobs` runs. A single copy can still go past
its own runs, by up to its cap; that run makes its job reserve its largest peak for 7 days (0033). In the replay,
as many jobs went above what they reserved under the old rule as under this one. At 33G the total was 0.1G more,
and at 20G and 14G it was less.
