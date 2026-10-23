# 0036. A job with 2 runs is predicted from its own largest peak

Amends [0033](0033-measured-admission.md), which predicted a job with fewer than `[sizing] min_jobs` runs as the
larger of its profile's p95 and its own largest peak.

## Context

A profile often mixes light and heavy jobs. On one host's first day under measured admission, a node profile ran
200M checks, 2G image builds and a 4.8G stack: its p95 was about 4.7G, so every job with fewer than 10 runs reserved
5.5G, even one whose 6 runs all peaked at 1.3G. Five runners filled a 33G slice that used about 8G, and jobs waited
15 minutes. Setting `min_jobs = 3` cut the waits, but `min_jobs` also gates the profile: with 3 light runs of the
profile, a new 2G image build was predicted at 512M.

## Decision

A job with at least 2 runs of its own (`admission::OWN_RUNS`) and fewer than `min_jobs` reserves its own largest
peak plus the margin (`Basis::JobMax`), once its profile has `min_jobs` runs. Below `min_jobs` the p95 of its runs is
their largest peak anyway (nearest rank, under 20 runs), so this applies the job's own rule sooner. A job with
1 run still takes the larger of that peak and its profile's p95: one light run may not be its heaviest. The rules
for no runs, runs at the cap, runs past what they reserved and the high-water mark are unchanged, as is the default
`min_jobs = 10`.

`runnerd simulate` on that host's history (74 jobs, `jvm` 2 and `node` 6 slots), measured admission's p95 wait and
jobs peaking above what they reserved:

| MemoryMax | Before, `min_jobs` 10 | Before, `min_jobs` 3 | Now, `min_jobs` 10 |
|---|---|---|---|
| 33G | 2m34s, 1 | 2m34s, 6 | 2m34s, 1 |
| 20G | 8m30s, 2 | 5m23s, 6 | 4m56s, 2 |
| 14G | 16m33s, 2 | 10m51s, 6 | 9m27s, 2 |

At 33G the slots bound the waits. The alternatives replayed, blending the profile's p95 in with a weight that
shrinks as the job's runs grow, or waiting for 3 runs of the job, had as many jobs above what they reserved and
waited as long or longer.

## Consequences

Young jobs reserve what they used, and a job's third run can go past its first two: one run did here, at 5.3G
after 400M and 658M, which the old rule under-predicted too. Such a run makes its job reserve its largest peak for
7 days (0033), and its container still stops at its cap.
