# 0038. Admission by CPU, from each job's average, off by default

Status: **Accepted** (2026-10-23).

## Context

Admission ([0033](0033-measured-admission.md)) packs runners by memory. CPU is shared: each container has a `cpus`
limit, and nothing holds their sum to the host's. When several CPU-heavy jobs run at once they share the CPUs and
each takes longer, which memory admission cannot see.

The job history keeps each job's CPU peak (the busiest 2-second sample) and its average (CPU time over run time).
On the owner's host they are far apart: image builds peak at 5–7 CPUs and average 0.5, a Gradle quality gate peaks
at 8 and averages 2.5. A peak lasts seconds.

## Decision

`[host] cpu_overcommit = <factor>` turns on a CPU budget: the slice's CPUs times the factor, at most 16. Absent, no
runner is admitted by CPU.

- The slice's CPUs come from systemd (`config::slice_cpus`): `EffectiveCPUs`, else `AllowedCPUs`, else the host's;
  fewer when `CPUQuotaPerSecUSec` allows less. They are read only while `cpu_overcommit` is set, at start and when
  a config that sets it is applied, which is refused if they cannot be read: a slice systemd describes oddly never
  stops a daemon without a CPU budget.
- Under `admission = "measured"` a runner reserves its job's predicted average CPU (`Predictor::predict_cpu`), by
  the same fallbacks as memory: the job's p95 with `[sizing] min_jobs` runs; with fewer, its own largest once
  `OWN_RUNS` succeeded, else the larger of that and its profile's p95; with none, its profile's largest; without
  `min_jobs` runs of the profile, its `cpus`. No margin and no copies rule: overcommit costs a slowdown, not a kill.
  Runs sampled before 0037 count, since page cache never changed their CPU.
- Under `admission = "caps"`, or before the history is read, a runner reserves its profile's `cpus`. Under caps every
  running runner counts at its container's CPU limit, also one admitted by its prediction before a reload, as
  memory does.
- CPU history is kept apart from memory history: a run whose peak is not learned (sampled before 0037) teaches its
  job's CPU without making the job known to memory predictions.
- A runner starts reserving the largest prediction among its repository's queued jobs, then its job's once known.
  Unlike memory, a waiting runner is never raised for a heavier queued job or stopped for one: a wrong guess costs a
  slowdown until the job ends.
- A reservation is at most the whole budget, so one runner can always start.
- The mirror, seed containers and shells reserve no CPU: they are rare, or idle most of the time.
- `plan` holds a job that had a slot and memory but not CPU (`Decision::HeldForCpu`). The engine logs it, the budget
  says a job waits (`api::CpuBudget`), and the runner that starts next for that repository records how long it waited
  (`api::CpuAdmission::waited`). Once a held repository has waited `CPU_PRIORITY_AFTER` (2 minutes), `plan` starts
  no younger job until its own fits: otherwise a stream of lighter jobs takes every CPU that frees up before it.
  `runnerd simulate` does not model this priority. Both fields are optional and absent without a CPU budget, so released clients read
  the status and records as before.

### Why the average

Reserving the CPU a job used over its run keeps the CPUs busy without letting the sum of the running jobs' work
exceed them; peaks that land together cost a short slowdown. Reserving peaks would admit one image build per 6 CPUs
where twenty fit by their averages, leaving the host mostly idle. A high percentile of a job's samples sits between
the two, but the history drops samples after 7 days and keeps the average for as long as the job's record.

### Why off by default

`runnerd simulate` replays the history with a CPU budget. On the owner's history (196 jobs, 32 CPUs, 33G), the
running jobs' averages never added up past 6.2 CPUs with the configured slots, 7 with 8 node slots and 7.7 with 12,
and the slice's samples went past 16 CPUs in 4 of 4,959. A budget at 1.0 held nothing back. Replayed against 8
CPUs, with 8 node slots, it held jobs 281 times and raised the p95 wait from 1m55s to 2m30s; at 1.5, 8 times and
2m08s.

On by default it would buy nothing on a host like the owner's and cost waits on a small one, and every job without
history would reserve its profile's whole `cpus`, a limit nobody configured: `slots × cpus` past the host's CPUs
would cut how many run at once. The documented starting value is 1.0.

## Consequences

A host short of CPU can trade a little wait for jobs that keep their speed. On a host with many CPUs it changes
nothing. The replay keeps each job's run time, so it shows what a CPU budget holds back, not how much sharing the
CPUs would have slowed the jobs: the evidence for a factor above 1 is the host's own CPU chart. A heavy job that
waited 2 minutes holds back every younger start until enough CPU drains, which can idle CPUs for up to the run time
of what is running.
