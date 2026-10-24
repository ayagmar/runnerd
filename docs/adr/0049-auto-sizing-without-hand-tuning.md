# 0049. Auto sizing changes CPUs and slots too, raises within the hour, and needs no hand-set ceiling

Status: **Accepted** (2026-10-24). Amends [0039](0039-auto-sizing.md) and [0046](0046-sizing-says-what-auto-sizing-does-next.md).

## Context

On the owner's host, 2026-10-24, a JVM profile's jobs sat at their 12-CPU cap (CPU peak p95 12.11) and waited 5m21s
at p95 for a slot, while its slots were idle most of the day. Auto sizing ([0039](0039-auto-sizing.md)) could not
help: it never changes `cpus`; it adds slots only up to `max_slots`, and none without it; it changes a profile at
most once per `[sizing] interval` (24 hours), raises and lowerings alike; a slot needs `min_jobs` runs of memory
history; and the advice line said "raise slots by hand" with auto sizing on. So the orchestrator set `cpus = 16`,
`slots = 3`, `max_slots = 4` and `cpu_overcommit = 1.5` by hand. The owner's rule: nobody hand-tunes these.

What makes the two directions different: a cap or slot too small costs every queued job time now, and a raise is
cheap to take back. A lowering can kill a job (memory) or slow every run (CPU), and the history that would show it
was wrong comes slowly.

## Decision

### Raise fast, lower slow

A change that lowers neither `memory` nor `cpus` can be made `[sizing] raise_interval` after the profile's last
change (default 1 hour, at most `interval`; a longer one counts as `interval`). A change that lowers either still
waits `interval` (24 hours). Under `caps`, a memory raise can leave room for fewer slots; it still counts as a raise,
as the jobs it is for were killed or held at their cap. Still one change per profile at a time, the last change
counted however old, pending or written, and each undoable.

A raise reads only the evidence since the profile's last change: the waits and slot use of the jobs queued after
it, and the CPU peaks of the runs after it at the profile's current `cpus`. Otherwise a 30-day p95 wait that a slot
already fixed would add one slot an hour until the budget ran out, and runs pinned at a cap the profile went back to,
by a lowering or an undo, would raise it again with nothing new: raise, lower, raise, every 24 hours.

### Slots without a hand-set ceiling

A profile whose jobs waited a minute or more at p95, or whose slots were busy half the time, since its last change
(3 jobs at least) gets one more slot per raise:

- under `measured`, when every profile's slots, at their jobs' p95 peaks (at their caps with fewer than `min_jobs`
  runs), and the mirror fit `MemoryMax`;
- under `caps`, when every profile's `slots × memory` and the mirror fit `MemoryMax` with it. A lowering adds as
  many as the memory the slots took holds, as the advice says.

Under `caps` with `cpu_overcommit` set, every slot added, by either rule, must also keep every profile's
`slots × cpus` within the CPU budget, each change against what the ones before it in the look left: under caps a
runner reserves its `cpus`, so a slot past the budget would only wait for CPU, and its waits would ask for another.
A lowering keeps its memory change and adds only the slots the budget holds.

`max_slots`, set, still bounds it. Without `max_slots` and without a `MemoryMax` nothing bounds slots, so none is
added. Slots are never lowered under what runs.

### CPU caps

`cpus` follows the CPU peak runs reach under it. Only runs whose container had the profile's current `cpus` count
(the runner's recorded CPU cap); a run under another cap says nothing about this one.

- Raise: with 3 or more such runs since the last change and their CPU peak at p95 within 5% of the cap, `cpus` goes up by `max_step`
  percent, at least 1, up to the slice's CPUs. Under `caps` admission with `cpu_overcommit` set, every profile's
  `slots × cpus` must also fit the CPU budget: a raise there trades concurrency for speed, which auto sizing does not
  decide.
- Lower: with `min_jobs` such runs and their largest peak plus `margin` at most half the cap, `cpus` goes down to
  that, rounded up to whole CPUs, by at most `max_step` percent at a time.

The history records the CPU cap of each change (schema version 10); a change recorded before it has none, and its
undo leaves `cpus` as they are.

### Young profiles

Slot and CPU raises need 3 runs, not `min_jobs`: they cost waits or slow runs when wrong, not kills. Memory changes
still need `min_jobs` runs both ways.

### The CPU budget stays a setting, off by default

`cpu_overcommit` is not derived. CPU admission reserves each job's average; the owner's slice has `CPUWeight=20`, so
the desktop keeps its CPUs whatever runs, and overcommitted jobs share CPUs and slow down rather than fail. A factor
tuned from the history would need run times measured against how much ran at once, which the history does not
separate from jobs changing. Without a budget nothing needs tuning: Linux shares the slice's CPUs by weight, and a
`cpus` cap raised by auto sizing costs the other jobs only when they all want the CPUs at once. A host that the CPU
chart shows saturated for long stretches can still opt in; `runnerd init` does not write it.

### What it says

The advice line no longer says "by hand" with auto sizing on: under `measured`, waiting jobs read "auto sizing adds
one when they fit the slice". `GET /v1/sizing`'s `auto_plan` gains `raise_interval`, and each hold `lower_until`:
`until` is now when the profile may be raised again, `lower_until` when it may be lowered. `runnerd init` writes
`[sizing] auto = true`.

A change's reason describes the `memory` and `slots` it writes, not the advice's: a step of `max_step` stops short
of the advice, and says what it goes toward. Before, a 25% step from 5.25G read "lower memory to 2.5G … 4 slots fit"
while it wrote 4G and kept 2 slots.

## Consequences

- The history schema goes to version 10 (`from_cpus`, `to_cpus` on `auto_sizing`); 0.0.9 refuses a database that
  new. `Resize` gains an optional `cpus`, which older clients skip and which `runnerctl sizing apply` leaves unset.
- A profile can gain a slot an hour while its jobs keep waiting: on the owner's host a waiting profile reaches what
  the budget fits within hours, not weeks. A wrong raise lasts until the 24-hour lowering, or an undo.
- An older client shows a hold until `until`, which is now the raise interval's end: still true, as no change comes
  before it.
- Without `max_slots` auto sizing can fill the slice's memory with one profile's slots when its jobs keep waiting;
  admission still starts only what fits. Set `max_slots` to keep room for another profile.
- Rejected: an auto-tuned `cpu_overcommit` (no signal to tune it from), a CPU limit per profile like `max_memory`
  (the slice's CPUs bound it; a bound can come when a host needs one), and a separate interval per kind of change
  (the risk is the direction, not the resource).
