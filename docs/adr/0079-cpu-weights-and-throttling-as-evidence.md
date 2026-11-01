# 0079. Runners share the slice's CPUs by their `cpus`, and throttling, not peaks, says the cap held a run back

Status: **Accepted** (2026-10-30). Amends [0075](0075-cpus-come-down-only-under-a-cap-no-burst-met.md).

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
- **Weights.** Each runner's container and a record's shell get CPU shares of 1024 a CPU of their `cpus`, within
  Docker's 2 to 262144, which Docker writes as cgroup v2's `cpu.weight`. They are siblings under the slice, so when
  its CPUs are all in use they share them by `cpus`; idle CPUs still go to whoever has work, up to each one's quota.
  The mirror keeps Docker's default weight, 100, about a 2.5-CPU runner's: at its 1 CPU it would weigh 39 beside a
  9-CPU runner's 352, and every job's image pulls go through it. The weights do nothing against programs outside the
  slice, which share the host with the slice by its own `CPUWeight`. Both isolation backends keep the field: sysbox
  sets only the runtime.
- **The cap held a run back** when its quota throttled it in `HELD` (15%) of its run or more: its throttled periods
  over its run's wall time in 100 ms periods (or the periods counted, if more). Not over `nr_periods`: the period
  timer stops after a period the cgroup used nothing in, so those count only its busy time, and an 8-minute job idle
  on its services between three 20-second compiles that want 20 CPUs is throttled in 550 of its 600 busy periods
  (92%) but 11% of its run, and lost about 30 s of 480. A throttled period loses only part of its 100 ms, so the share
  is more than the time lost: the gate lost 17% of its run at 9 CPUs and was throttled in 28% of it, 21% at 13 and
  16% at 19, and 15% keeps it climbing while the 8-minute job stays. The job's own line, "in 18% of its busy time",
  keeps the busy periods: it says how often the job hit the cap while it worked, not what sizing reads. A run without
  the figure, as one from before schema 17 or on a kernel that keeps none, is judged by its peak as 0075 does: missing
  is not read as never throttled, or every profile would read as unthrottled after the upgrade and the measured raise
  would stop. Each run says what it can, so runs with and without the figure count together.
- **Sizing.** Under `measured` admission, 3 runs of one job the cap held back raise `cpus` by `max_step`, within the
  slice and the CPU budget as 0075 decided; the peak rule stays only for runs without the figure.
- **A raise goes on only while the one before helped.** Before raising, the look finds the job's latest runs at other
  `cpus`; when those were fewer, a raise came before, and with 3 runs or more there, the job's runs at the current
  `cpus` must have taken less time at the median, or been throttled in less of their run at the median, than at the
  `cpus` before. When neither did, the raise is refused, and `runnerctl sizing` says the raise from 9 to its 13 CPUs
  did not help it, with both medians. The throttle rule shares the thread-pool argument against a ceiling below:
  Gradle at 13 CPUs starts 13 workers, its runnable threads still pass the quota, and it stays throttled at every
  cap, so without this the raise would climb to the slice, adding weight and memory at each step without making the
  job faster. A job whose width does not follow the quota, as the gate's 23.5 CPUs, gets faster with each raise and
  climbs until its quota stops throttling it. With no raise before (the first, or after a lowering) or too few runs
  at the `cpus` before, nothing says it did not help, and the raise is made. The averages' raise (`BOUND`) is
  unchanged.
- Under `caps`, a
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
- When the slice is full, a large runner takes CPU from small ones it shared with evenly before; the mirror keeps the
  default weight.
- Runner containers made before the upgrade keep the default weight until they are replaced, which warm runners are
  within their idle time, so for a while the slice mixes weighted and unweighted siblings.
- A profile whose job stays throttled however many `cpus` it gets stops one raise above where it started throttling,
  and `runnerctl sizing` says why on every look until its runs say otherwise or a hand edit starts the evidence over.
