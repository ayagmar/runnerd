# 0033. Admission by measured peaks, behind a flag

## Context

A runner starts only when its profile's `memory` fits the slice's `MemoryMax` beside the caps of everything
running, and the config check holds `slots × memory` over all profiles, with the mirror, to `MemoryMax`. Caps are
set for a profile's heaviest job, so most jobs use a fraction of what they reserve: a node build that peaks at 2G
holds 7G. Jobs wait for memory the running ones will never touch.

The job history ([ADR 0032](0032-sizing-advice-from-sampled-peaks.md)) has each job's sampled peak. A cap is a hard
kill limit per container; `MemoryMax` is one for the whole slice. Neither changes if admission reserves less than
the caps, so the risk of reserving less is the slice reaching `MemoryMax` when several jobs peak at once, which
the kernel answers by reclaiming and then killing inside the slice.

## Decision

`[host] admission = "measured"` turns it on; `"caps"`, today's rule, stays the default.

Under measured admission a runner reserves a prediction instead of its cap (`admission::Predictor`):

- A job is keyed by repository, workflow and name. With `[sizing] min_jobs` runs or more, it gets its p95 sampled
  peak plus `[sizing] margin` percent, rounded up to 256M.
- With fewer runs, the larger of its profile's p95 and its own largest peak, each plus the margin: a rare heavy job
  is not predicted at its light neighbours' size.
- With too few runs of either, the cap.
- A run of the job that peaked within 5% of its cap, or in whose container the kernel killed a process for memory
  (`memory.events` `oom_kill`, sampled with the container's memory), makes the job unpredictable: its cap, for 7
  days.
- A prediction is never above the cap.

A runner is registered before GitHub gives it a job, and GitHub may give it any of its repository's queued jobs, so
it reserves the largest prediction among them. The engine reads the history into a new predictor at most once a
minute.

Two checks guard the slice beside the predictions. While the slice's sampled memory is at 90% of `MemoryMax` or
more, nothing starts. Without a sample from the last three sampling intervals, runners reserve their caps. A
runner a restart adopts reserves its cap.

The config check then holds one runner of each profile plus the mirror to `MemoryMax`, not `slots × memory` over
all of them; `slots` bounds how many of a profile run at once. Measured admission refuses a slice without a
`MemoryMax`.

Each runner's row in the job history keeps how it was admitted (`api::Admission`: policy, what it reserved, what
that went by, the budget left and the slice's memory then), and a job's record shows its runner's.

`runnerd simulate` replays the history through both policies before anyone turns the flag on. On a synthetic month
(about 1,500 jobs of three repositories, bursts and merge trains, a long tail of run times, 2–3% of runs at their
cap) against 33G, measured admission with 3 jvm and 4 node slots halved the p95 wait of the configured caps, and with
4 and 8 slots cut it to a quarter. Every running job's peak added up at the busiest moment, which is more than the
slice ever holds, stayed at 27.5G or less; against 20G, at 18.2G.

## Consequences

More jobs run at once on the same memory when their peaks are well below their caps. The slice can now reach
`MemoryMax` where caps could not: when jobs exceed their predictions together, faster than one tick, or while
the slice is under 90% and a burst of starts lands. The kernel then reclaims page cache and, past that, kills a
process in the slice. The caps still bound each container; the high-water mark bounds how far the predictions are
trusted.

A job killed for memory between two samples looks like a low peak; only the `oom_kill` counter catches it, and
only while its container exists. Those kills make the job unpredictable, so the cost of a wrong prediction is
paid once a week, not on every run.

Predictions follow the jobs as they change: a job that grows heavier is predicted from its new runs once they
finish, and is admitted at its old prediction until then.

Admission by caps is unchanged, and stays the default.
