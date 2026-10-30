# 0068. Auto sizing moves `cpus` on each job's run average, not on CPU peaks

Status: **Accepted** (2026-10-26), amended by [0075](0075-cpus-come-down-only-under-a-cap-no-burst-met.md) on the
lowering. Amends [0049](0049-auto-sizing-without-hand-tuning.md), where it states the CPU
rule, and [0046](0046-sizing-says-what-auto-sizing-does-next.md).

## Context

[0049](0049-auto-sizing-without-hand-tuning.md) raised a profile's `cpus` by `max_step` when 3 runs since its last
change peaked within 5% of the cap at p95, and lowered them when the largest peak plus `margin` was half the cap or
less. Peaks are the highest 2-second interval of a run. A parallel build (Gradle, Kotlin, R8, turbo, vitest) bursts to
whatever cap it gets, so on the owner's host, 2026-10-24/25, the rule only went up: the JVM profile 16 → 20 → 25 → 31
CPUs in four hours, the Node profile 8 → 22 in sixteen. With three runs, p95 is the largest, so one burst at the cap
was enough. The lowering never fired, as peaks sat at the cap whatever it was.

The same runs averaged far under it. At the 20 → 25 raise the Android quality gate peaked at 20.0 CPUs and averaged
6.5. Across the history (68 jobs with 3 or more successful measured runs at one cap), no job's median run averaged
more than 37% of its cap, and no single run more than 50%. Runs at 31 CPUs were not faster than at 16; the history
cannot say whether they were slower because of it, since the code and the job mix changed too. Memory rose in the
same days, partly before any CPU raise, so this decision makes no claim about memory.

## Decision

### Evidence

A `cpus` change reads each run's average CPU (CPU time over the run, which the history already records), grouped by
job: profile, repository, workflow and name. A run counts when it succeeded, has a measured average (none, or 0 from a
run sampled once, is unknown, never zero), its container had the profile's current `cpus`, and it was queued since the
profile's last change and since the daemon started running these `cpus`.

That second boundary comes from the `profile.changed` events the history already stores: one when the daemon starts
with a profile, and one after each reload or restart that changes its CPUs, memory, slots or image. The run of events
since the last one with other `cpus` starts at the boundary. So a hand edit, a reload or an undo away and back starts
the evidence over even when no job ran at the cap in between, and a restart reads it back. A config the daemon has
not announced yet, as after an edit made while it was down, has no evidence until it does. Pruning keeps each
profile's latest event, and drops older ones after `[history] events` (30 days, the sizing window): a boundary it
dropped can only move later, which reads fewer runs, never older ones. No runs since then: nothing moves. A look waits for the store's queued writes
before it reads the events, so an announcement still in the queue counts. One case stays open: an announcement whose
write fails is lost, and an edit away and back across it reads the runs before; the failed write is logged.

### Raise

One job with 3 such runs averaging 70% of the cap or more at the median raises `cpus` by `max_step` percent, at
least 1, up to the slice's CPUs, through the same joint accounting with slots and the CPU budget as before. One heavy
job is enough; a profile mean would hide it behind its light jobs.

70% because a run is more than its build: checkout, cache restore and upload barely use a CPU, so a job averaging 70%
of the cap spent most of its build at it. The history's heaviest job sits at 37%. The rule misses a build with a short
parallel phase and a low average; that phase runs slower than it could, which is accepted.

A raise the slice's CPUs or the CPU budget leave no room for is not written. `GET /v1/sizing`'s `auto_plan` lists it
in `refused` with why, and every client shows "No CPU raise" and the reason for that profile, as core words it, beside
whatever the same look still changes in its memory or slots.

### Lowering

`interval` after the last change, with `min_jobs` such runs, `cpus` come down toward twice the heaviest job's p95
average, rounded up to whole CPUs, at least 1, by at most `max_step` percent a change. A job whose runs at the cap
have no measured average keeps the profile where it is: it may be the one that needs the CPUs. Twice: a burst at
twice the run's average still runs uncapped, and while the jobs keep the demand they showed, the one that set the
target then averages half the cap at p95, far under the raise's 70%, so the lowering does not ask for its raise back.
The heaviest job's p95, not a mean, so a burst of cheap or failed jobs does not pull the target down.

Contention is not ruled out. A day in which other programs hold every run of the heaviest job to a fraction of its
demand looks like a job that needs less, and the profile comes down a step; once the host is free the job sits at
the lower cap and is raised back. The step and the interval bound what that costs, a quarter of the CPUs for a day or
so; nothing in the averages tells contention from a lighter job, and a trial that could is below. Replayed on the owner's history, the old rule makes each of the seven raises the host saw and this one
none; from where they left JVM and Node, it steps 31 → 24 toward 13 CPUs and 22 → 17 toward 11, a day after their
last change. A memory change resets that day too, so a profile whose memory keeps rising waits for it to settle.

### No duration trial

A raise is not kept or taken back on whether runs got faster. The history has no branch, commit, cache state or
competing load for a run, so a before and after of one job compares different work; a trial that reverts would need
its own state, owned per resource, through crash, undo and memory changes made meanwhile. The average gate removes
the raises the history shows were wrong without it. A trial can come when runs carry what makes them comparable.

### Memory

Unchanged. `memory` still sizes on the 30-day largest peak, and no lowering comes while a run in those 30 days was
killed or met its cap; admission's 7-day hold for an unpredictable job is separate. So lower `cpus` lower memory, if
at all, only once those windows pass with lower peaks: weeks, not a day.

## Consequences

- No schema change. `AutoPlan` gains `refused`, skipped when empty; 0.0.16 ignores it.
- A profile far over its jobs' needs comes down a step a day without a hand edit; one that a lowering made too small
  for a short-burst build stays there. Undo it, or set `cpus` by hand.
- A day of host contention can cost the heaviest profile one step down and one back up (above).
- A lowering can free CPU budget that memory or wait sizing then gives to slots; a later raise that no longer fits
  is refused and shown, not written.
- Rejected: peaks at a higher percentile or over more runs (a burst reaches any cap), quota throttling counters (not
  recorded, and they count how often a burst met the cap, not whether more CPUs speed a run up), and a duration trial
  (above).
