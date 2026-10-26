# 0068. Auto sizing moves `cpus` on each job's run average, not on CPU peaks

Status: **Accepted** (2026-10-26). Amends [0049](0049-auto-sizing-without-hand-tuning.md), where it states the CPU
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
profile's last change and since the profile's last run under another cap. A hand edit, a reload or an undo starts the
evidence over as a change does, without a record of its own. No runs since then: nothing moves.

### Raise

One job with 3 such runs averaging 70% of the cap or more at the median raises `cpus` by `max_step` percent, at
least 1, up to the slice's CPUs, through the same joint accounting with slots and the CPU budget as before. One heavy
job is enough; a profile mean would hide it behind its light jobs.

70% because a run is more than its build: checkout, cache restore and upload barely use a CPU, so a job averaging 70%
of the cap spent most of its build at it. The history's heaviest job sits at 37%. The rule misses a build with a short
parallel phase and a low average; that phase runs slower than it could, which is accepted.

A raise the slice's CPUs or the CPU budget leave no room for is not written. `GET /v1/sizing`'s `auto_plan` lists it
in `refused` with why, and every client shows "No CPU raise" and the reason for that profile, as core words it.

### Lowering

`interval` after the last change, with `min_jobs` such runs, `cpus` come down toward twice the heaviest job's p95
average, rounded up to whole CPUs, at least 1, by at most `max_step` percent a change. Twice: a burst at twice the
run's average still runs uncapped, and the job that set the target then averages half the cap at p95, far under the
raise's 70%, so a lowering does not ask for its raise back. The heaviest job's p95, not a mean, so a burst of cheap or
failed jobs, or other programs slowing the runs, does not pull the target down; and a day between steps leaves time
to see it. Replayed on the owner's history, the old rule makes each of the seven raises the host saw and this one
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
- A lowering can free CPU budget that memory or wait sizing then gives to slots; a later raise that no longer fits
  is refused and shown, not written.
- Rejected: peaks at a higher percentile or over more runs (a burst reaches any cap), quota throttling counters (not
  recorded, and they count how often a burst met the cap, not whether more CPUs speed a run up), and a duration trial
  (above).
