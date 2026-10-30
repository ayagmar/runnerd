# 0079. Runners share the slice's CPUs by their `cpus`, and throttling, not peaks, says the cap held a run back

Status: **Accepted** (2026-10-30). Amends [0075](0075-cpus-come-down-only-under-a-cap-no-burst-met.md), where it reads
a run's peak as the cap's cost.

## Context

A runner's `cpus` is a hard quota (`nano_cpus`, the cgroup's `cpu.max`), and every container in the slice had the same
default weight. On the owner's 32-thread host, mostly idle, the Android quality gate ran about 90 s of its 517 s
(17%) slower at 9 CPUs than at 16. Nothing measured that cost: auto sizing read it from 2-second peaks, and 0075
raises `cpus` under `measured` admission when 3 runs peaked within 95% of the cap. A peak at the cap says a burst
reached it, not what the cap cost; one such interval raised a profile before (the 16 → 31 ratchet).

The kernel counts what the quota did in each cgroup's `cpu.stat`: `nr_periods`, the 100 ms periods it had work in
(the period timer stops while it is idle), and `nr_throttled`, those in which it used its quota up and waited for the
next. `throttled_usec` adds up the time each CPU's run queue of the cgroup sat throttled (`unthrottle_cfs_rq` in
`kernel/sched/fair.c`), so for a job spread over many CPUs it passes the wall time and cannot be set against the run.
`cpu.pressure`'s `some` total is the time at least one of its tasks could run and waited for a CPU: the quota's holds
and contention with the other runners and programs.

The brief also proposed a ceiling above the reservation: a CPU limit some factor over `cpus`.

## Decision

- **Measure.** The sampler reads each runner's `nr_periods`, `nr_throttled` and `cpu.pressure` `some` total every
  2 s beside its CPU time, and the history keeps them per sample (schema 17). A counter the kernel does not keep (no
  cpu controller on the cgroup, no pressure stall information) is `None`, and the sample is still taken. A job's
  figures run from its runner's last sample before it, or zero at its container's start, to its last sample, as its
  CPU time does, so a warm runner's idle time is not the job's and a restart mid-job keeps them. A figure that one of
  those samples lacks is unknown, never 0. The final record keeps them (`jobs.cpu_periods`, `cpu_throttled`,
  `cpu_stalled`). `JobMetrics` carries them as additive fields, and core words them for every client:
  `held at its CPU cap in 18% of its busy time, waited for a CPU 30% of its run`.
- **Weights.** Each runner's container, a record's shell and the mirror get CPU shares of 1024 a CPU of their `cpus`,
  within Docker's 2 to 262144, which Docker writes as cgroup v2's `cpu.weight`. They are siblings under the slice, so
  when its CPUs are all in use they share them by `cpus`; idle CPUs still go to whoever has work, up to each one's
  quota. The weights do nothing against programs outside the slice, which share the host with the slice by its own
  `CPUWeight`. Both isolation backends keep the field: sysbox sets only the runtime.
- **The cap held a run back** when its quota throttled it in `HELD` (10%) of its busy periods or more. 17% of the
  gate's run was lost at 9 CPUs, and its busy periods are no more than its run's, so its share of them is higher
  (28% in the model of its runs); a cap touched by a few bursts a minute stays far under 10%. A run without the figure,
  as one from before schema 17 or on a kernel that keeps none, is judged by its peak as 0075 does: missing is not read
  as never throttled, or every profile would read as unthrottled after the upgrade and the measured raise would stop.
  Each run says what it can, so runs with and without the figure count together.
- **Sizing.** Under `measured` admission, 3 runs of one job the cap held back raise `cpus` by `max_step`, within the
  slice and the CPU budget as 0075 decided; the peak rule stays only for runs without the figure. Under `caps`, a
  lowering holds while any run it reads was held back, whatever its peak, and still never goes under the runs'
  highest peak. A run slowed by contention, waiting for a CPU without its quota throttling it, raises nothing: the
  slice is short, not the profile. `cpu.pressure` is shown, not read by sizing.
- **No ceiling above `cpus`.** Rejected for now:
  - Under `measured` admission it is what the cap already is: a running job reserves its predicted average, and the
    throttle rule raises a cap that costs its runs time. A factor on top would compound with that raise and drive
    every measured profile to the slice's CPUs.
  - Sizing, insights and the dashboard read `runners.cpu_cap` as the cap (`runs_at` matches runs by it, peaks are
    compared with it, the dashboard writes "X of N CPUs"). A ceiling needs a second stored figure and a rework of
    each.
  - Tools size their pools from the quota: the JVM's `availableProcessors` (Gradle's default `--max-workers`, the
    Kotlin daemon), Rust's `available_parallelism` (cargo's default `-j`), Go's `GOMAXPROCS` from 1.25, .NET. A
    higher ceiling spawns more workers and raises memory within a run, which measured memory admission learns only
    after `min_jobs` runs; until then the memory cap kills them, as when 31 Gradle workers ran the Kotlin daemon out
    of heap. `nproc` reads the CPU affinity mask, not the quota, so it already shows the host's CPUs. An environment
    variable with the reservation is read by no tool, and `-XX:ActiveProcessorCount` through `JAVA_TOOL_OPTIONS`
    prints on every JVM's stderr and overrides the workflows' own settings.
  - Revisit under `caps` admission, where `cpus` is both the reservation and the quota, with this measurement's data.

## Consequences

- Schema 17 adds three nullable columns to `samples` and to `jobs`. Rows from before keep NULL, read as unknown.
- A v0.0.22 client ignores the new `JobMetrics` fields; a new one shows the line where it shows a job's CPU: `runnerctl
  job`, the TUI's job page and the dashboard's.
- Under `measured`, a run whose 2-second peak touched the cap but that its quota did not throttle no longer raises
  `cpus`. A profile the old peaks raised stays where it is: nothing lowers under `measured`.
- Under `caps`, a run whose peak met the cap without being throttled no longer holds a lowering, but the lowering
  still stops at that peak, rounded up, so it rarely moves.
- When the slice is full, a large runner takes CPU from small ones it shared with evenly before, and the mirror weighs
  as a runner with its `cpus` does.
