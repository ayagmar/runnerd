# 0033. Admission by measured peaks, behind a flag

Amended by [0037](0037-memory-the-kernel-cannot-reclaim.md): samples leave out all reclaimable memory, and a run
met its cap when its container's `memory.events` say so, not when its peak came within 5% of it.

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
- With fewer runs, the larger of its profile's p95 and its own largest peak, plus the margin: a rare heavy job is
  not predicted at its light neighbours' size.
- With no run of its own, its profile's largest peak plus the margin: a new workflow, or a matrix job whose name
  changes, may be the profile's heaviest.
- With too few runs of the profile as well, or when every run of the job was cancelled, the cap.
- A run of the job that peaked within 5% of its cap, or in whose container the kernel killed a process for memory
  (`memory.events` `oom_kill`, sampled with the container's memory, and Docker's `OOMKilled` when the container
  stops), makes the job unpredictable: its cap, for 7 days. Cancelled runs count for this, not for their peaks.
- A run that peaked past what its runner reserved has its job reserve the larger of its own largest peak and that
  run's peak, plus the margin, for 7 days, a cancelled run's peak included: the job that filled the slice is
  learned, not only the one the kernel killed.
- A prediction is never above the cap.

A runner is registered before GitHub gives it a job, and GitHub may give it any of its repository's queued jobs, so
it starts reserving the largest prediction among them, and while it waits it follows that largest one up. A waiting
runner raised past what the budget has free is stopped, as one that waited `IDLE_TIMEOUT` is, until the rest fits:
the heavier job waits for a runner that fits. Once its log capture finds its job, it reserves that job's prediction.
The engine reads the history into a new predictor at most once a minute.

GitHub assigns a job between two ticks: until the next one, its runner counts at what it reserved before. A heavier
job taken that way is counted only afterwards, which the cap of its container bounds.

A running runner counts against the budget at the larger of what it reserves and its last sampled memory, so a job
past its prediction leaves less for the next starts. When that sample, or the slice's, is older than three sampling
intervals, the runner counts at its cap: what it uses is not known. Under `admission = "caps"` every runner counts at its cap,
also one admitted by its prediction before a reload.

Two checks guard the slice beside the predictions. While the slice's sampled memory is at 90% of `MemoryMax` or
more, nothing starts. Without a sample from the last three sampling intervals, runners reserve their caps. A
runner a restart adopts reserves its cap. The mirror runs with an `oom_score_adj` of -500, so the kernel kills a
job's process before it.

The config check then holds one runner of each profile plus the mirror to `MemoryMax`, not `slots × memory` over
all of them; `slots` bounds how many of a profile run at once. Measured admission refuses a slice without a
`MemoryMax`.

Each runner's row in the job history keeps how it was admitted (`api::Admission`: policy, what it reserves now,
what that goes by, the budget left and the slice's memory at the start), and a job's record shows its runner's.

`runnerd simulate` replays the history through both policies before anyone turns the flag on, by the same rules: a
runner starts reserving the largest prediction among its repository's waiting jobs, then its job's own, and a
running job counts at the larger of its prediction and its peak. On a synthetic month (about 1,500 jobs of three
repositories, bursts and merge trains, a long tail of run times, 2–3% of runs at their cap) against 33G, measured
admission with 3 jvm and 4 node slots brought the p95 wait to 42–58% of the configured caps', and with 4 and 8
slots to 28–44%. Every running job's peak added up at the busiest moment, which is more
than the slice ever holds, stayed at 27.5G or less; against 20G, at 17.2G.

That sum is a lower bound on what the daemon could overlap, not a bound on it. The replay holds each running job at
its peak for its whole run, where the daemon sees what jobs use while they warm up and starts more beside them;
and it leaves out start latency, seeds, shells and cancelled jobs.

## Consequences

More jobs run at once on the same memory when their peaks are well below their caps. The slice can now reach
`MemoryMax` where caps could not: when jobs exceed their predictions together, faster than one tick, or while
the slice is under 90% and a burst of starts lands. The kernel then reclaims page cache and, past that, kills a
process in the slice. The caps still bound each container; the high-water mark bounds how far the predictions are
trusted.

A job's memory between two samples is not seen: a spike shorter than the interval looks like a low peak. A kill
for memory is still caught, by the `oom_kill` counter while the container runs and by Docker when it stops. Those
kills make the job unpredictable, so the cost of a wrong prediction is paid once a week, not on every run.

Predictions follow the jobs as they change: a job that grows heavier is predicted from its new runs once they
finish, and is admitted at its old prediction until then.

Admission by caps is unchanged, and stays the default.
